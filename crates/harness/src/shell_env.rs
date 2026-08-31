//! Resolve the user's login-shell PATH for GUI-launched processes.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static CACHE: OnceLock<Option<OsString>> = OnceLock::new();

/// Return the PATH reported by the user's login shell, cached for this
/// process. A disabled or unavailable shell produces no extra PATH entries.
pub fn login_shell_path() -> Option<&'static OsStr> {
    CACHE.get_or_init(capture).as_deref()
}

fn capture() -> Option<OsString> {
    if std::env::var_os("ZERON_NO_LOGIN_SHELL").is_some_and(|value| !value.is_empty()) {
        return None;
    }

    let shell = std::env::var_os("SHELL")
        .filter(|value| !value.is_empty())
        .or_else(|| Some(OsString::from("/bin/sh")))?;
    let mut child = std::process::Command::new(shell)
        .args(["-l", "-i", "-c", "printf '__ZERON_PATH__%s\\n' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("ZERON_RESOLVING_ENVIRONMENT", "1")
        .env("TERM", "dumb")
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }

    let mut output = Vec::new();
    child.stdout.take()?.read_to_end(&mut output).ok()?;
    let marker = b"__ZERON_PATH__";
    let start = output
        .windows(marker.len())
        .position(|window| window == marker)?;
    let path = &output[start + marker.len()..];
    let end = path.iter().position(|byte| *byte == b'\n')?;
    (!path[..end].is_empty())
        .then(|| OsString::from(String::from_utf8_lossy(&path[..end]).into_owned()))
}
