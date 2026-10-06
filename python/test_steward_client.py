"""steward_client against a real stewardd in a scratch environment.

    cargo build -p stewardd && python3 -m unittest python/test_steward_client.py

STEWARDD names the daemon binary (default: target/debug/stewardd).
"""

from __future__ import annotations

import asyncio
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from steward_client import (  # noqa: E402
    Located,
    AsyncClient,
    Client,
    ConnectionLost,
    Event,
    Gap,
    StewardError,
)

HERE = os.path.dirname(os.path.abspath(__file__))
STEWARDD = os.environ.get(
    "STEWARDD", os.path.join(HERE, "..", "target", "debug", "stewardd")
)


class Daemon:
    def __init__(self) -> None:
        self.tmp = os.path.realpath(tempfile.mkdtemp(prefix="steward-test-"))
        self.run = os.path.join(self.tmp, "run")
        self.data = os.path.join(self.tmp, "data")
        os.makedirs(self.run)
        os.makedirs(os.path.join(self.data, "films"))
        self.config = os.path.join(self.tmp, "settings.toml")
        with open(self.config, "w") as f:
            f.write(
                f'db = "{self.tmp}/index.db"\n[[root]]\npath = "{self.data}"\n'
            )
        self.proc: subprocess.Popen | None = None

    @property
    def content(self) -> str:
        return os.path.join(self.run, "steward", "content.socket")

    @property
    def api(self) -> str:
        return os.path.join(self.run, "steward", "api.socket")

    def start(self) -> None:
        env = {
            **os.environ,
            "XDG_RUNTIME_DIR": self.run,
            "STEWARD_CONFIG": self.config,
            "HOME": self.tmp,
        }
        self.proc = subprocess.Popen([STEWARDD], env=env, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                with Client(self.api, timeout=5) as c:
                    s = c.status()
                    if s["indexed"] and not s["scanning"]:
                        return
            except ConnectionLost:
                pass
            time.sleep(0.05)
        raise RuntimeError("stewardd did not come up")

    def stop(self) -> None:
        if self.proc is not None:
            self.proc.terminate()
            self.proc.wait()
            self.proc = None

    def cleanup(self) -> None:
        self.stop()
        shutil.rmtree(self.tmp)

    def write(self, rel: str, data: bytes) -> str:
        path = os.path.join(self.data, rel)
        with open(path, "wb") as f:
            f.write(data)
        return path


def data(n: int, seed: int) -> bytes:
    return bytes((i * 31 + seed) & 0xFF for i in range(n))


def bep52_root(payload: bytes) -> str:
    """BEP 52 pieces root: SHA-256 of 16 KiB blocks, padded to a power of
    two with zero hashes, then a binary tree."""
    block = 1 << 14
    leaves = [
        hashlib.sha256(payload[i : i + block]).digest()
        for i in range(0, len(payload), block)
    ]
    n = 1
    while n < len(leaves):
        n *= 2
    leaves += [bytes(32)] * (n - len(leaves))
    while len(leaves) > 1:
        leaves = [
            hashlib.sha256(leaves[i] + leaves[i + 1]).digest()
            for i in range(0, len(leaves), 2)
        ]
    return "btv2:" + leaves[0].hex()


class ClientTest(unittest.TestCase):
    def setUp(self) -> None:
        self.d = Daemon()
        self.film = self.d.write("films/a.bin", data(3 << 20, 1))
        self.film_id = bep52_root(data(3 << 20, 1))
        self.d.start()

    def tearDown(self) -> None:
        self.d.cleanup()

    def test_sync_primitives(self) -> None:
        with Client(self.d.content, timeout=30) as c:
            with self.assertRaises(StewardError) as e:
                c.reload()
            self.assertEqual(e.exception.type, "forbidden")

            [r] = c.resolve([self.film_id])
            self.assertEqual(r.state, "unknown")

            [i, bad] = c.inspect([self.film, "/etc/hostname"])
            self.assertTrue(i.ok)
            self.assertEqual(i.id, self.film_id)
            self.assertEqual(bad.error["type"], "not_under_root")

            [r, m] = c.resolve([(self.film_id, 3 << 20), (self.film_id, 1)], True)
            self.assertEqual(r.state, "present")
            self.assertEqual([o.path for o in r.online], [self.film])
            self.assertEqual(m.state, "mismatch")
            self.assertEqual(len(c.piece_layer(self.film_id)), 3 * 32)

            self.assertEqual(c.locate("a.bin", mode="exact", kind="file"), [self.film])
            found = c.locate("a.bin", mode="exact", check="exists")
            self.assertIsInstance(found, Located)
            self.assertEqual((found.paths, found.stale), ([self.film], []))

            v = c.verify(self.film_id, self.film, "test")
            self.assertEqual((v.state, v.current), ("unchanged", self.film_id))

        # A name that isn't UTF-8 round-trips: as str with a surrogate, and as bytes.
        raw = os.path.join(os.fsencode(self.d.data), b"films", b"caf\xe9.bin")
        with open(raw, "wb") as f:
            f.write(data(5000, 3))
        with Client(self.d.content, timeout=30) as c:
            [i] = c.inspect([raw])
            self.assertTrue(i.ok, i.error)
            self.assertEqual(os.fsencode(i.path), raw)
            [hit] = c.locate("caf", mode="substring")
            self.assertEqual(os.fsencode(hit), raw)
            self.assertEqual(c.stat(hit).size, 5000)
            [r] = c.resolve([i.id])
            self.assertEqual(os.fsencode(r.observations[0].path), raw)

        with Client(self.d.api, timeout=30) as admin:
            self.assertIn("roots", admin.settings())

    def test_async_concurrency_events_and_restart(self) -> None:
        asyncio.run(self._async_scenario())

    async def _async_scenario(self) -> None:
        c = AsyncClient(self.d.content)
        async with c:
            # Concurrent calls on one connection.
            inspected, status = await asyncio.gather(
                c.inspect([self.film]), c.status()
            )
            self.assertEqual(inspected[0].id, self.film_id)
            self.assertIn("indexed", status)

            async with c.events(ids=[self.film_id], max_backoff=0.2) as events:
                moved = os.path.join(self.d.data, "films", "renamed.bin")
                os.rename(self.film, moved)
                await c.inspect([moved])
                e = await asyncio.wait_for(anext_event(events, "content.moved"), 30)
                self.assertEqual((e.data["from"], e.data["to"]), (self.film, moved))

                # Restart the daemon under the open stream: calls in flight
                # fail, the next call reconnects, the stream reports a gap.
                self.d.stop()
                with self.assertRaises(ConnectionLost):
                    await c.status()
                self.d.start()
                self.assertIn("indexed", await c.status())
                gap = await asyncio.wait_for(events.__anext__(), 30)
                self.assertIsInstance(gap, Gap)
                self.assertEqual(gap.reason, "restarted")

                # Events flow again after the restart.
                back = os.path.join(self.d.data, "films", "back.bin")
                os.rename(moved, back)
                await c.inspect([back])
                e = await asyncio.wait_for(anext_event(events, "content.moved"), 30)
                self.assertEqual(e.data["to"], back)


async def anext_event(events, name: str) -> Event:
    async for e in events:
        if isinstance(e, Event) and e.name == name:
            return e
    raise AssertionError("stream ended")


if __name__ == "__main__":
    unittest.main()
