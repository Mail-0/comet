//! Launch and reconnect to the local Keiki engine.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The soonest a second engine is started after the first.
const SPAWN_SPACING: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct Config {
    pub port: u16,
    pub data_dir: PathBuf,
    bin: Option<PathBuf>,
    home: Option<PathBuf>,
    spawn: bool,
    /// Keiki.app's engine, managed as Keiki.app manages it (the beta): its
    /// bundle's, started through crates/launch; it restarts into a newer
    /// bundle by itself.
    managed: bool,
}

/// Why no engine can be used.
pub type Refusal = String;

/// The file in an engine's data dir holding the build of the engine this
/// app started there last (`zeron-engine --build-id`): an engine reporting
/// that build is one this app started.
const STARTED: &str = "zgx-started-build";

impl Config {
    /// The engine this app would start now, if it may start one.
    pub fn startable(&self) -> Option<&Path> {
        if self.spawn {
            self.bin.as_deref()
        } else {
            None
        }
    }

    /// Whether this is Keiki.app's engine, which follows its bundle by
    /// itself (no "older than the one on disk" of this app's to judge).
    pub fn follows_bundle(&self) -> bool {
        self.managed
    }
}

/// Which build `bin` is (`zeron-engine --build-id`), if it says; an engine
/// from before build ids doesn't.
pub fn build_of(bin: &Path) -> Option<String> {
    let out = std::process::Command::new(bin)
        .arg("--build-id")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let id = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (out.status.success() && !id.is_empty() && !id.contains(' ')).then_some(id)
}

/// The build of the engine this app last started with `config`'s data.
pub fn started_build(config: &Config) -> Option<String> {
    let id = std::fs::read_to_string(config.data_dir.join(STARTED)).ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

impl Config {
    pub fn from_env() -> Result<Self, Refusal> {
        let var = |key: &str| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self::from_vars(var, &home, std::env::current_exe().ok().as_deref())
    }

    fn from_vars(
        var: impl Fn(&str) -> Option<String>,
        home: &Path,
        exe: Option<&Path>,
    ) -> Result<Self, Refusal> {
        let port = match var("ZERON_ZGX_ENGINE_PORT").or_else(|| var("ZERON_IPC_PORT")) {
            Some(port) => port
                .parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| "Engine IPC port must be between 1 and 65535".to_string())?,
            None => zeron_cli::DEFAULT_IPC_PORT,
        };
        let data_dir = var("ZERON_ZGX_ENGINE_DATA")
            .map(|path| expand(&path, home))
            .unwrap_or_else(|| {
                zeron_cli::paths::resolve_data_dir(|key| match key {
                    "HOME" => Some(home.as_os_str().to_owned()),
                    key => var(key).map(Into::into),
                })
            });
        let managed = cfg!(feature = "beta");
        let bin = var("ZERON_ZGX_ENGINE_BIN")
            .map(|path| expand(&path, home))
            .or_else(|| {
                if managed {
                    exe.and_then(zeron_launch::engine_executable)
                } else {
                    Some(
                        Path::new(env!("CARGO_MANIFEST_DIR"))
                            .join("../../target/debug/zeron-engine"),
                    )
                }
            });
        Ok(Self {
            port,
            data_dir,
            bin,
            home: var("ZERON_ZGX_ENGINE_HOME").map(|path| expand(&path, home)),
            spawn: var("ZERON_ZGX_ENGINE_SPAWN").as_deref() != Some("0"),
            managed,
        })
    }

    pub fn describe(&self) -> String {
        format!("port {}, data {}", self.port, self.data_dir.display())
    }
}

fn expand(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(path),
    }
}

/// Starts engines for `config`, no more often than [`SPAWN_SPACING`].
pub struct Spawner {
    last: Option<Instant>,
    said_missing: bool,
    said_held: bool,
}

impl Spawner {
    pub fn new() -> Self {
        Self {
            last: None,
            said_missing: false,
            said_held: false,
        }
    }

    /// Whether the last look found another engine holding the data dir.
    pub fn held(&self) -> bool {
        self.said_held
    }

    /// Whether an engine this app started is likely still coming up: started
    /// less than [`SPAWN_SPACING`] ago, so it's dialled again soon rather
    /// than less and less often.
    pub fn starting(&self) -> bool {
        self.last.is_some_and(|at| at.elapsed() < SPAWN_SPACING)
    }

    /// Nothing answers on the port: start an engine, if this app may and
    /// it's been long enough since the last. Whether one was started. An
    /// engine that holds the data dir without serving yet is on its way in or
    /// out: none is started beside it, and one follows once it has let go.
    pub fn start_if_due(&mut self, config: &Config) -> bool {
        if !config.spawn || self.last.is_some_and(|at| at.elapsed() < SPAWN_SPACING) {
            return false;
        }
        let Some(bin) = &config.bin else {
            if !self.said_missing {
                self.said_missing = true;
                eprintln!(
                    "zeron-zgx: engine: nothing answers on {} and there's no engine to start: {}",
                    config.describe(),
                    if config.managed {
                        "this app has no Zeron Engine beside it"
                    } else {
                        "build one (`cargo build -p zeron --bin zeron-engine` at the repository's root) \
                         or set ZERON_ZGX_ENGINE_BIN"
                    }
                );
            }
            return false;
        };
        if zeron_launch::data_dir_held(&config.data_dir) {
            if !self.said_held {
                self.said_held = true;
                eprintln!(
                    "zeron-zgx: engine: another holds {} and isn't serving; waiting for it",
                    config.data_dir.display()
                );
            }
            return false;
        }
        self.said_held = false;
        self.last = Some(Instant::now());
        // Started on a thread of its own: creating the process waits for the
        // system to map (and Windows' antivirus to scan) the engine's
        // executable, a few hundred milliseconds the first time a new
        // version runs, which the window's first frame shouldn't wait for.
        let (config, bin) = (config.clone(), bin.clone());
        let started = std::thread::Builder::new()
            .name("engine-start".into())
            .spawn(move || {
                let started = if config.managed {
                    launch(&config)
                } else {
                    spawn(&config, &bin)
                };
                match started {
                    Ok(()) => eprintln!(
                        "zeron-zgx: engine: started {} ({})",
                        bin.display(),
                        config.describe()
                    ),
                    Err(error) => {
                        eprintln!("zeron-zgx: engine: {} didn't start: {error}", bin.display())
                    }
                }
            });
        if let Err(error) = started {
            eprintln!("zeron-zgx: engine: couldn't start a thread to start it: {error}");
            self.last = None;
            return false;
        }
        true
    }
}

/// Keiki.app's engine, started as Keiki.app starts it (crates/launch).
fn launch(config: &Config) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let child = zeron_launch::launch(
        &zeron_launch::LaunchConfig {
            data_dir: config.data_dir.clone(),
            ipc_port: config.port,
        },
        &exe,
    )?;
    zeron_launch::reap(child);
    Ok(())
}

/// A development engine, detached (its own process group, no inherited
/// pipes), so this app closing leaves it running; it logs to its data dir.
fn spawn(config: &Config, bin: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    // Which build it is, kept (an engine that reports it is this app's), as
    // its reaper asks below: off this thread, since asking runs the binary.
    // Until then none is kept, so a check meanwhile reads it as not ours.
    let started = config.data_dir.join(STARTED);
    let _ = std::fs::remove_file(&started);
    let mut command = std::process::Command::new(bin);
    command
        .env("ZERON_DATA_DIR", &config.data_dir)
        .env("ZERON_IPC_PORT", config.port.to_string())
        // Not the shipping app's: no app to open from a banner.
        .env_remove("ZERON_APP")
        // Nor a chat's (this app run from a Zeron terminal): an engine that
        // carries a chat's origin marker lends it to everything it runs, so
        // the shipping engine's preview scan would probe its agents'
        // sign-in callbacks (Gemini CLI's OAuth ends on the first stray
        // request).
        .env_remove("ZERON_ORIGIN")
        .env_remove("ZERON_EDGE_TOKEN")
        .env_remove("ZERON_IPC_TOKEN")
        .env_remove("ZERON_IPC_TOKEN_FILE")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(home) = &config.home {
        command.env("HOME", home);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command.spawn()?;
    // Its build kept, then it reaped when it exits, whenever that is.
    let bin = bin.to_path_buf();
    std::thread::spawn(move || {
        if let Some(id) = build_of(&bin) {
            let _ = std::fs::write(&started, id);
        }
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_engine_uses_keiki_local_data() {
        let home = tempfile::tempdir().unwrap();
        let config = Config::from_vars(|_| None, home.path(), None).unwrap();
        assert_eq!(config.port, zeron_cli::DEFAULT_IPC_PORT);
        assert_eq!(config.data_dir, home.path().join(".keiki"));
    }

    #[test]
    fn explicit_engine_overrides_are_kept() {
        let home = tempfile::tempdir().unwrap();
        let config = Config::from_vars(
            |key| match key {
                "ZERON_ZGX_ENGINE_DATA" => Some("~/sandbox".into()),
                "ZERON_ZGX_ENGINE_PORT" => Some("28001".into()),
                "ZERON_ZGX_ENGINE_SPAWN" => Some("0".into()),
                _ => None,
            },
            home.path(),
            None,
        )
        .unwrap();
        assert_eq!(config.port, 28001);
        assert_eq!(config.data_dir, home.path().join("sandbox"));
        assert!(!config.spawn);
    }

    #[test]
    fn invalid_engine_ports_are_rejected() {
        for value in ["zero", "0", "65536"] {
            assert!(
                Config::from_vars(
                    |key| (key == "ZERON_ZGX_ENGINE_PORT").then(|| value.into()),
                    Path::new("/nonexistent"),
                    None
                )
                .is_err()
            );
        }
    }

    #[test]
    fn retired_account_settings_cannot_select_a_different_workspace() {
        let home = tempfile::tempdir().unwrap();
        let config = Config::from_vars(|key| match key {
            "ZERON_ORG_ID" | "ZERON_USER_ID" | "ZERON_EDGE_TOKEN" => Some("retired-account".into()),
            _ => None,
        }, home.path(), None).unwrap();
        assert_eq!(config.data_dir, home.path().join(".keiki"));
        assert_eq!(config.port, zeron_cli::DEFAULT_IPC_PORT);
    }

}
