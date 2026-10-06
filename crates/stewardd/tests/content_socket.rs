//! The daemon end to end over its two sockets, in a scratch environment:
//! capability split, resolve, inspect, verify and content events.

use std::fs;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use steward_proto::Client;

struct Daemon {
    child: Child,
    run: PathBuf,
    data: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn start() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let (run, data) = (base.join("run"), base.join("data"));
        fs::create_dir_all(&run).unwrap();
        fs::create_dir_all(data.join("films")).unwrap();
        fs::write(data.join("films/a.bin"), bytes(3 << 20, 1)).unwrap();
        fs::write(data.join("films/b.bin"), bytes(100_000, 2)).unwrap();
        let config = base.join("settings.toml");
        fs::write(
            &config,
            format!(
                "db = {:?}\n[[root]]\npath = {:?}\n",
                base.join("index.db"),
                data
            ),
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_stewardd"))
            .env("XDG_RUNTIME_DIR", &run)
            .env("STEWARD_CONFIG", &config)
            .env("HOME", &base)
            .spawn()
            .unwrap();
        let d = Self {
            child,
            run,
            data,
            _tmp: tmp,
        };
        // Wait for the first scan of the root to finish.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "stewardd did not come up");
            if let Ok(mut c) = d.admin() {
                let s = c.call("status", json!({})).unwrap();
                if !s["indexed"].as_array().unwrap().is_empty() && s["scanning"] == false {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        d
    }

    fn admin(&self) -> std::io::Result<Client> {
        Client::connect_to(&self.run.join("steward/api.socket"))
    }

    fn content(&self) -> Client {
        let c = Client::connect_to(&self.run.join("steward/content.socket")).unwrap();
        c.set_timeout(Some(Duration::from_secs(30))).unwrap();
        c
    }
}

fn bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn id_of(path: &Path) -> String {
    let h = steward_contentid::hash_file(path).unwrap().unwrap();
    format!("btv2:{}", h.id.to_hex())
}

/// Read events until one matches, failing after a timeout.
fn wait_for(events: &mut steward_proto::Events, what: impl Fn(&Value) -> bool) -> Value {
    for msg in events {
        let msg = msg.expect("event stream");
        if msg["method"] == "event" && what(&msg["params"]) {
            return msg["params"].clone();
        }
    }
    panic!("event stream ended");
}

#[test]
fn content_socket_end_to_end() {
    let d = Daemon::start();
    let a = d.data.join("films/a.bin");
    let a_id = id_of(&a);

    // Administration is refused on content.socket, allowed on api.socket.
    let mut c = d.content();
    let e = c.call("reload", json!({})).unwrap_err();
    assert_eq!(e.kind, "forbidden");
    let settings = d.admin().unwrap().call("settings", json!({})).unwrap();
    let fs = &settings["roots"][0]["fs"];
    assert!(fs["bytes_total"].as_u64().unwrap() > 0, "{fs}");
    assert!(fs["type"].is_string() && fs["mount"].is_string(), "{fs}");
    // data/, films/, a.bin, b.bin
    let stat = c.call("stat", json!({ "path": d.data })).unwrap();
    assert_eq!(stat["total_items"], 4);
    let e = c.call("no_such_method", json!({})).unwrap_err();
    assert_eq!(e.kind, "method_not_found");

    // Unknown before anything is hashed.
    let r = c
        .call("resolve", json!({ "contents": [{ "id": a_id }] }))
        .unwrap();
    assert_eq!(r[0]["state"], "unknown");

    let mut events = d
        .content()
        .subscribe(None, Some(vec![a_id.clone()]))
        .unwrap();
    assert_eq!(events.start["complete"], true);

    // inspect hashes files now; directories and outsiders are handled per item.
    let r = c
        .call(
            "inspect",
            json!({ "paths": [a, d.data.join("films"), "/etc/hostname", d.data.join("nope")] }),
        )
        .unwrap();
    assert_eq!(r[0]["id"], a_id.as_str());
    assert_eq!(r[0]["kind"], "file");
    assert_eq!(r[0]["size"], 3 << 20);
    assert_eq!(r[1]["kind"], "dir");
    assert_eq!(r[2]["error"]["type"], "not_under_root");
    assert_eq!(r[3]["error"]["type"], "not_found");
    let e = wait_for(&mut events, |e| e["name"] == "content.observed");
    assert_eq!(e["data"]["path"], a.to_str().unwrap());

    // resolve: present, with the size guard and the verification layer.
    let r = c
        .call(
            "resolve",
            json!({ "contents": [{ "id": a_id, "size": 3 << 20 }, { "id": a_id, "size": 5 }],
                    "recheck": true }),
        )
        .unwrap();
    assert_eq!(r[0]["state"], "present");
    assert_eq!(r[0]["layer"], true);
    assert_eq!(r[0]["observations"][0]["path"], a.to_str().unwrap());
    assert_eq!(r[0]["observations"][0]["online"], true);
    assert_eq!(r[1]["state"], "mismatch");
    let layer = c
        .call("piece_layer", json!({ "id": a_id, "piece_size": 1 << 20 }))
        .unwrap();
    assert_eq!(layer["layer"].as_str().unwrap().len(), 3 * 64);

    // A rename, noticed by inspecting the new path, is a move.
    let moved = d.data.join("films/renamed.bin");
    fs::rename(&a, &moved).unwrap();
    let r = c.call("inspect", json!({ "paths": [moved] })).unwrap();
    assert_eq!(r[0]["id"], a_id.as_str());
    let e = wait_for(&mut events, |e| e["name"] == "content.moved");
    assert_eq!(e["data"]["from"], a.to_str().unwrap());
    assert_eq!(e["data"]["to"], moved.to_str().unwrap());
    let r = c
        .call("resolve", json!({ "contents": [{ "id": a_id }] }))
        .unwrap();
    let paths: Vec<_> = r[0]["observations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(paths, [moved.to_str().unwrap()]);

    // Bytes changed behind the same size and mtime: only a reread tells.
    let mtime = fs::metadata(&moved).unwrap().modified().unwrap();
    let mut changed = fs::read(&moved).unwrap();
    changed[5] ^= 0xff;
    fs::write(&moved, &changed).unwrap();
    fs::File::options()
        .write(true)
        .open(&moved)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let r = c
        .call(
            "resolve",
            json!({ "contents": [{ "id": a_id }], "recheck": true }),
        )
        .unwrap();
    assert_eq!(r[0]["state"], "present", "stat alone can't see it");
    let v = c
        .call(
            "verify",
            json!({ "id": a_id, "path": moved, "reason": "test: mismatch" }),
        )
        .unwrap();
    assert_eq!(v["state"], "changed");
    assert_eq!(v["current"], id_of(&moved).as_str());
    let e = wait_for(&mut events, |e| e["name"] == "content.lost");
    assert_eq!(e["data"]["reason"], "changed");
    assert_eq!(e["data"]["path"], moved.to_str().unwrap());
    let r = c
        .call(
            "resolve",
            json!({ "contents": [{ "id": a_id }], "recheck": true }),
        )
        .unwrap();
    assert_eq!(r[0]["state"], "absent");
    assert_eq!(r[0]["observations"], json!([]));

    // verify of an unchanged file, and of a missing one.
    let b = d.data.join("films/b.bin");
    let b_id = id_of(&b);
    let v = c.call("verify", json!({ "id": b_id, "path": b })).unwrap();
    assert_eq!(v["state"], "unchanged");
    let v = c
        .call(
            "verify",
            json!({ "id": b_id, "path": d.data.join("films/gone.bin") }),
        )
        .unwrap();
    assert_eq!(v["state"], "gone");

    // status shows what is running, including this test's connections.
    let activity = c.call("status", json!({})).unwrap()["activity"].clone();
    assert!(activity["connections"].as_u64().unwrap() >= 2, "{activity}");
    assert_eq!(activity["subscribers"], 1);
    assert!(activity["event_seq"].as_u64().unwrap() > 0);
    assert!(activity["reading"].is_array());
    let status = d.admin().unwrap().call("status", json!({})).unwrap();
    assert_eq!(status["daemon"]["version"], steward_proto::VERSION);
    assert!(status["daemon"]["db_bytes"].as_u64().unwrap() > 0);
    assert!(status["daemon"]["uptime_secs"].is_u64());
    let scans = status["recent_scans"].as_array().unwrap();
    assert!(
        scans
            .iter()
            .any(|s| s["kind"] == "full" && s["root"] == json!(d.data))
    );
    assert_eq!(status["schedule"][0]["path"], json!(d.data));
    assert!(status["schedule"][0]["next"].as_f64().unwrap() > 0.0);
    // verify of a changed file logged a warning, kept for status.
    let problems = status["problems"].as_array().unwrap();
    assert!(
        problems
            .iter()
            .any(|p| p["level"] == "warning" && p["message"].as_str().unwrap().contains("holds")),
        "{problems:?}"
    );

    // locate: exact and typed; with check, gone results are noticed, and
    // rescan brings the index up to date with the renamed file.
    let found = c
        .call(
            "locate",
            json!({ "pattern": "b.bin", "mode": "exact", "kind": "file" }),
        )
        .unwrap();
    assert_eq!(found, json!([b.to_str().unwrap()]));
    fs::rename(&b, d.data.join("films/b2.bin")).unwrap();
    let r = c
        .call(
            "locate",
            json!({ "pattern": "^b2?\\.bin$", "mode": "regex", "check": "exists" }),
        )
        .unwrap();
    assert_eq!(r["paths"], json!([]));
    assert_eq!(r["stale"], json!([b.to_str().unwrap()]));
    let r = c
        .call(
            "locate",
            json!({ "pattern": "^b2?\\.bin$", "mode": "regex", "check": "rescan" }),
        )
        .unwrap();
    assert_eq!(
        r["paths"],
        json!([d.data.join("films/b2.bin").to_str().unwrap()])
    );
    assert_eq!(r["stale"], json!([]));
    assert_eq!(
        r["rescanned"],
        json!([d.data.join("films").to_str().unwrap()])
    );
    let e = c
        .call("locate", json!({ "pattern": "(", "mode": "regex" }))
        .unwrap_err();
    assert_eq!(e.kind, "invalid_params");

    // A new subscriber can replay everything since the start.
    let replay = d.content().subscribe(Some(0), None).unwrap();
    assert_eq!(replay.start["complete"], true);
    assert_eq!(replay.start["epoch"], events.start["epoch"]);
    let mut replay = replay;
    let first = wait_for(&mut replay, |_| true);
    assert_eq!(first["seq"], 1);
    // A seq the daemon never reached is a gap.
    let ahead = d.content().subscribe(Some(1_000_000), None).unwrap();
    assert_eq!(ahead.start["complete"], false);
}

#[test]
fn names_that_are_not_utf8_travel_as_surrogate_escapes() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::ffi::OsStrExt as _;
    let d = Daemon::start();
    let mut raw = d.data.join("films").into_os_string().into_vec();
    raw.extend_from_slice(b"/caf\xe9.bin");
    let path = PathBuf::from(std::ffi::OsString::from_vec(raw.clone()));
    fs::write(&path, bytes(5000, 3)).unwrap();
    let wire_name = format!("{}/caf\\udce9.bin", d.data.join("films").display());

    let s = std::os::unix::net::UnixStream::connect(d.run.join("steward/content.socket")).unwrap();
    let mut w = s.try_clone().unwrap();
    let mut lines = BufReader::new(s).lines();
    let mut ask = |method: &str, params: &str| -> String {
        writeln!(
            w,
            r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{params}}}"#
        )
        .unwrap();
        lines.next().unwrap().unwrap()
    };
    // In: a path sent as Python's json.dumps would. Out: the same escape.
    let reply = ask("inspect", &format!(r#"{{"paths":["{wire_name}"]}}"#));
    assert!(reply.contains(r"caf\udce9.bin"), "{reply}");
    assert!(reply.contains(r#""error":null"#), "{reply}");
    let reply = ask("locate", r#"{"pattern":"caf","mode":"substring"}"#);
    assert!(reply.contains(r"caf\udce9.bin"), "{reply}");
    let reply = ask("stat", &format!(r#"{{"path":"{wire_name}"}}"#));
    assert!(reply.contains(r#""size":5000"#), "{reply}");

    // The Rust client gets the bytes back exactly.
    let mut c = d.content();
    let hits = c.locate("caf".into(), 10).unwrap();
    assert_eq!(steward_proto::wire::decode(&hits[0]), raw);
    let e = c.stat(path.clone()).unwrap();
    assert_eq!(
        steward_proto::wire::to_path(&e.path).as_os_str().as_bytes(),
        raw
    );
}

#[test]
fn locate_reports_rescans_as_progress_before_answering() {
    use std::io::{BufRead, BufReader, Write};
    let d = Daemon::start();
    fs::rename(d.data.join("films/b.bin"), d.data.join("films/b2.bin")).unwrap();
    let mut s =
        std::os::unix::net::UnixStream::connect(d.run.join("steward/content.socket")).unwrap();
    let msg = json!({ "jsonrpc": "2.0", "id": 9, "method": "locate", "params": {
        "pattern": "b", "check": "rescan", "progress": true } });
    s.write_all(format!("{msg}\n").as_bytes()).unwrap();
    let mut stages = Vec::new();
    for line in BufReader::new(s).lines() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if v["method"] == "progress" {
            assert_eq!(v["params"]["id"], 9);
            assert!(v["params"]["message"].is_string());
            stages.push(v["params"]["stage"].as_str().unwrap().to_string());
            continue;
        }
        assert_eq!(v["id"], 9);
        assert_eq!(
            v["result"]["rescanned"],
            json!([d.data.join("films").to_str().unwrap()])
        );
        break;
    }
    assert_eq!(stages, ["rescanning", "rescanned"]);
}

#[test]
fn requests_on_one_connection_are_concurrent_and_matched_by_id() {
    use std::io::{BufRead, BufReader, Write};
    let d = Daemon::start();
    let mut s =
        std::os::unix::net::UnixStream::connect(d.run.join("steward/content.socket")).unwrap();
    let a = d.data.join("films/a.bin");
    // Pipelined: a hashing inspect, then a cheap status; both answer.
    for (id, method, params) in [
        (1, "inspect", json!({ "paths": [a] })),
        (2, "status", json!({})),
    ] {
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        s.write_all(format!("{msg}\n").as_bytes()).unwrap();
    }
    s.write_all(b"not json\n").unwrap();
    let mut seen = std::collections::HashMap::new();
    let mut lines = BufReader::new(s).lines();
    while seen.len() < 3 {
        let v: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        seen.insert(v["id"].to_string(), v);
    }
    assert_eq!(seen["1"]["result"][0]["id"], id_of(&a).as_str());
    assert!(seen["2"]["result"]["indexed"].is_array());
    assert_eq!(seen["null"]["error"]["code"], -32700);
}
