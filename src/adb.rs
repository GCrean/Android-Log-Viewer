//! Thin wrapper around the `adb` command line tool.

use anyhow::{bail, Context as _, Result};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;

#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub serial: String,
    /// `device`, `offline`, `unauthorized`, ...
    pub state: String,
    pub description: String,
}

impl Device {
    pub fn is_ready(&self) -> bool {
        self.state == "device"
    }
    pub fn label(&self) -> String {
        if self.description.is_empty() {
            self.serial.clone()
        } else {
            format!("{} ({})", self.description, self.serial)
        }
    }
}

pub enum Msg {
    Line(u64, String),
    Ended(u64, Option<String>),
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "adb.exe"
    } else {
        "adb"
    }
}

/// Locate adb: explicit override, SDK environment variables, PATH, then common install dirs.
pub fn find_adb(custom: Option<&str>) -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(c) = custom.filter(|c| !c.trim().is_empty()) {
        candidates.push(c.into());
    }
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(root) = std::env::var_os(var) {
            candidates.push(Path::new(&root).join("platform-tools").join(exe_name()));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join(exe_name()));
        }
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join("Android/Sdk/platform-tools").join(exe_name()));
        candidates.push(home.join("Library/Android/sdk/platform-tools").join(exe_name()));
        candidates.push(home.join("AppData/Local/Android/Sdk/platform-tools").join(exe_name()));
    }
    candidates.push("/opt/homebrew/bin/adb".into());
    candidates.push("/usr/local/bin/adb".into());
    candidates.push("/usr/bin/adb".into());
    candidates.into_iter().find(|p| p.is_file()).unwrap_or_else(|| exe_name().into())
}

fn command(adb: &Path) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(adb);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

fn run(adb: &Path, args: &[&str]) -> Result<String> {
    let out = command(adb)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run `{}` - is adb installed?", adb.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("{}", stderr.trim().lines().last().unwrap_or("adb failed"));
    }
    Ok(stdout)
}

pub fn list_devices(adb: &Path) -> Result<Vec<Device>> {
    Ok(parse_devices(&run(adb, &["devices", "-l"])?))
}

fn parse_devices(out: &str) -> Vec<Device> {
    out.lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?.to_string();
            let state = parts.next()?.to_string();
            let mut model = None;
            let mut product = None;
            for p in parts {
                if let Some(m) = p.strip_prefix("model:") {
                    model = Some(m.replace('_', " "));
                } else if let Some(m) = p.strip_prefix("product:") {
                    product = Some(m.to_string());
                }
            }
            Some(Device { serial, state, description: model.or(product).unwrap_or_default() })
        })
        .collect()
}

/// `adb connect host[:port]` for network devices.
pub fn connect(adb: &Path, addr: &str) -> Result<String> {
    let out = run(adb, &["connect", addr.trim()])?;
    let msg = out.trim().to_string();
    if msg.starts_with("failed") || msg.contains("unable to connect") || msg.contains("cannot connect") {
        bail!("{msg}");
    }
    Ok(msg)
}

/// Spawns `adb logcat` and streams its lines to `tx` from a reader thread.
/// The returned child can be killed to stop the stream.
pub fn start_logcat(adb: &Path, serial: &str, session: u64, tx: Sender<Msg>) -> Result<Child> {
    let mut child = command(adb)
        .args(["-s", serial, "logcat", "-v", "threadtime"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run `{}` - is adb installed?", adb.display()))?;
    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;

    let stderr_thread = std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut buf = Vec::new();
        let mut last = None;
        while let Ok(n) = reader.read_until(b'\n', &mut buf) {
            if n == 0 {
                break;
            }
            let s = String::from_utf8_lossy(&buf).trim().to_string();
            if !s.is_empty() {
                last = Some(s);
            }
            buf.clear();
        }
        last
    });

    std::thread::spawn(move || {
        let mut reader = BufReader::with_capacity(64 * 1024, stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf).trim_end_matches(['\r', '\n']).to_string();
                    if line.is_empty() || line.starts_with("--------- beginning of") {
                        continue;
                    }
                    if tx.send(Msg::Line(session, line)).is_err() {
                        return;
                    }
                }
            }
        }
        let err = stderr_thread.join().ok().flatten();
        let _ = tx.send(Msg::Ended(session, err));
    });
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices() {
        let out = "List of devices attached\n25025524701043         device product:EM45 model:EM45 device:EM45 transport_id:1\n192.168.1.5:5555 unauthorized transport_id:2\n\n";
        let d = parse_devices(out);
        assert_eq!(d.len(), 2);
        assert!(d[0].is_ready());
        assert_eq!(d[0].description, "EM45");
        assert!(!d[1].is_ready());
    }
}
