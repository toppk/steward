//! `steward service`, `steward ui` and `steward upgrade`: running steward on
//! this machine. The service is a systemd user unit that runs
//! `steward daemon`; `steward` writes it, and only ever rewrites or removes
//! a unit it wrote.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub const UNIT: &str = "steward.service";
const MARK: &str = "# Written by `steward service install`, which may rewrite or remove it.";
const INSTALLER: &str = "https://toppk.github.io/steward/install.sh";

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"),
        PathBuf::from,
    )
}

fn unit_path() -> PathBuf {
    config_home().join("systemd/user").join(UNIT)
}

/// This program's own path, as the unit should run it.
fn this_exe() -> Result<PathBuf> {
    std::env::current_exe().context("finding this program's path")
}

fn unit_text(exe: &Path) -> String {
    let exe = exe.display().to_string();
    let exe = if exe.contains(char::is_whitespace) {
        format!("\"{exe}\"")
    } else {
        exe
    };
    format!(
        "{MARK}
[Unit]
Description=steward file index service
Documentation=https://toppk.github.io/steward/

[Service]
ExecStart={exe} daemon
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
Nice=10
IOSchedulingClass=idle

[Install]
WantedBy=default.target
"
    )
}

fn systemctl(args: &[&str]) -> Result<bool> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("running systemctl --user (is systemd managing this session?)")?;
    Ok(status.success())
}

fn systemctl_quiet(args: &[&str]) -> bool {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn ours(path: &Path) -> Result<bool> {
    Ok(std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?
        .starts_with(MARK))
}

pub fn install() -> Result<()> {
    let exe = this_exe()?;
    if exe.components().any(|c| c.as_os_str() == "target") {
        eprintln!(
            "note: {} looks like a build directory; the service will run it from there",
            exe.display()
        );
    }
    let legacy = config_home().join("systemd/user/stewardd.service");
    if legacy.exists() {
        bail!(
            "{} (from an earlier `just install`) is installed. Remove it first:\n  \
             systemctl --user disable --now stewardd && rm {}",
            legacy.display(),
            legacy.display()
        );
    }
    let path = unit_path();
    if path.exists() && !ours(&path)? {
        bail!(
            "{} exists and was not written by steward; not touching it",
            path.display()
        );
    }
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
    std::fs::write(&path, unit_text(&exe))
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {} (runs {} daemon)", path.display(), exe.display());
    if !systemctl(&["daemon-reload"])? {
        bail!("systemctl --user daemon-reload failed");
    }
    // `enable --now` doesn't restart a running service onto a new binary.
    let action = if systemctl_quiet(&["is-active", UNIT]) {
        "restart"
    } else {
        "start"
    };
    if !systemctl(&["enable", UNIT])? || !systemctl(&[action, UNIT])? {
        bail!("could not enable and {action} {UNIT}");
    }
    println!("{UNIT} is enabled and running; it starts with each login session");
    if !lingering() {
        println!(
            "to keep it running while you are logged out: loginctl enable-linger {}",
            std::env::var("USER").unwrap_or_default()
        );
    }
    Ok(())
}

fn lingering() -> bool {
    let user = std::env::var("USER").unwrap_or_default();
    Path::new("/var/lib/systemd/linger").join(user).exists()
}

pub fn uninstall() -> Result<()> {
    let path = unit_path();
    if !path.exists() {
        println!("{} is not installed", UNIT);
        return Ok(());
    }
    if !ours(&path)? {
        bail!(
            "{} was not written by steward; not touching it",
            path.display()
        );
    }
    systemctl(&["disable", "--now", UNIT])?;
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    systemctl(&["daemon-reload"])?;
    println!("stopped, disabled and removed {UNIT}; the index and settings are untouched");
    Ok(())
}

/// `start`, `stop`, `restart` or `status`, passed through to systemctl.
pub fn control(action: &str) -> Result<()> {
    if !unit_path().exists() {
        bail!("{UNIT} is not installed; run `steward service install`");
    }
    let ok = systemctl(&[action, UNIT])?;
    // `status` exits non-zero for a stopped service, which is an answer.
    if !ok && action != "status" {
        bail!("systemctl --user {action} {UNIT} failed");
    }
    Ok(())
}

pub fn logs(follow: bool) -> Result<()> {
    let mut cmd = Command::new("journalctl");
    cmd.args(["--user", "-u", UNIT, "-n", "200"]);
    if follow {
        cmd.arg("-f");
    }
    exec(cmd)
}

/// Replace this process with `cmd`.
fn exec(mut cmd: Command) -> Result<()> {
    use std::os::unix::process::CommandExt as _;
    let err = cmd.exec();
    Err(err).with_context(|| format!("running {:?}", cmd.get_program()))
}

/// Launch the desktop app: `steward-ui` beside this program, else on PATH.
pub fn ui(args: Vec<std::ffi::OsString>) -> Result<()> {
    let beside = this_exe()?.with_file_name("steward-ui");
    let program = if beside.exists() {
        beside
    } else {
        PathBuf::from("steward-ui")
    };
    let mut cmd = Command::new(&program);
    cmd.args(args);
    exec(cmd).with_context(|| {
        "steward-ui is not installed; install it with \
         `curl -fsSL https://toppk.github.io/steward/install.sh | STEWARD_UI=1 sh`"
            .to_string()
    })
}

/// Install the latest release over this one, with the same installer the
/// website offers, then restart the service if it is running.
pub fn upgrade() -> Result<()> {
    let exe = this_exe()?;
    if steward_proto::VERSION == "dev" {
        bail!(
            "{} is a development build; upgrade it from source (`just install`), or \
             install a release with `curl -fsSL {INSTALLER} | sh`",
            exe.display()
        );
    }
    let dir = exe.parent().context("this program has no directory")?;
    let script = std::env::temp_dir().join(format!("steward-install-{}.sh", std::process::id()));
    let fetched = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--output",
        ])
        .arg(&script)
        .arg(INSTALLER)
        .status()
        .context("running curl")?;
    if !fetched.success() {
        bail!("could not download {INSTALLER}");
    }
    let ran = Command::new("sh")
        .arg(&script)
        .env("STEWARD_INSTALL_DIR", dir)
        .status()
        .context("running the installer");
    let _ = std::fs::remove_file(&script);
    if !ran?.success() {
        bail!("the installer failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_runs_this_program_and_is_marked_ours() {
        let text = unit_text(Path::new("/home/me/.local/bin/steward"));
        assert!(text.starts_with(MARK));
        assert!(text.contains("\nExecStart=/home/me/.local/bin/steward daemon\n"));
        let spaced = unit_text(Path::new("/home/me/my bin/steward"));
        assert!(spaced.contains("ExecStart=\"/home/me/my bin/steward\" daemon"));
    }
}
