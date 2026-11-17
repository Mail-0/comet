//! zeron — headed by default; `zeron headless` runs the engine alone. Both use
//! the same device-local workspace profile.

mod daemon;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "zeron", about = "Multi-device controller for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Open a Keiki conversation URL.
    #[arg(value_name = "URL")]
    open_url: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine without a UI.
    Headless,
    /// Manage `zeron headless` as a background service (launchd / systemd --user).
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    Install,
    Uninstall,
    Start,
    Stop,
    Restart,
    Status,
}

/// mimalloc: system malloc (macOS libmalloc especially) never returns the
/// streaming churn's high-water pages, so transient allocation became
/// permanent RSS (docs/memory-plan.md §1).
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Long-running modes log at info, one-shot CLI commands at warn (RUST_LOG
    // overrides either).
    // loro's internal block-encode diagnostics log at info and flood
    // journald on every snapshot export — enough to fill a disk on a
    // long-running headless host. Quiet them by default (RUST_LOG still
    // overrides the whole filter).
    let long_running = matches!(&cli.command, None | Some(Command::Headless));
    let default_filter = if long_running {
        "info,loro_internal=warn,loro=warn"
    } else {
        "warn"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());
    // Long-running modes mirror stdout logging to {data_dir}/logs — a headed
    // app launched from Finder has no visible stdout, which left diagnostics
    // unavailable even though the engine logs the exact failure line. One file
    // per launch, previous
    // launch kept as `.old`.
    let log_file = if long_running {
        let mode = if cli.command.is_some() {
            "headless"
        } else {
            "headed"
        };
        open_log_file(mode)
    } else {
        None
    };
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let registry = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer());
        match log_file {
            Some(file) => registry
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(std::sync::Arc::new(file)),
                )
                .init(),
            None => registry.init(),
        }
    }

    match cli.command {
        Some(Command::Headless) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let engine = zeron_engine::Engine::new(engine_config_from_env());
                engine.run().await
            })
        }
        Some(Command::Daemon { command }) => match command {
            DaemonCommand::Install => daemon::install(&engine_config_from_env().data_dir),
            DaemonCommand::Uninstall => daemon::uninstall(),
            DaemonCommand::Start => daemon::start(),
            DaemonCommand::Stop => daemon::stop(),
            DaemonCommand::Restart => daemon::restart(),
            DaemonCommand::Status => daemon::status(),
        },
        None => {
            // Headed: the UI probes ZERON_IPC_PORT and connects to a running
            // daemon, or embeds the engine in-process (ARCHITECTURE §1).
            zeron_ui::run_app(zeron_ui::UiConfig {
                data_dir: std::env::var_os("ZERON_DATA_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(dirs_data_dir),
                ipc_port: std::env::var("ZERON_IPC_PORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(27654),
                default_harness: zeron_ui::HarnessId::Copilot,
                initial_url: cli.open_url,
                keiki_api_url: std::env::var("KEIKI_API_URL")
                    .ok()
                    .filter(|url| !url.trim().is_empty())
                    .unwrap_or_else(|| "https://onkeiki.com".into()),
            });
            Ok(())
        }
    }
}

/// Environment-resolved engine configuration shared by `headless` and the
/// daemon, so both use the same local workspace.
fn engine_config_from_env() -> zeron_engine::EngineConfig {
    zeron_engine::EngineConfig {
        data_dir: std::env::var_os("ZERON_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(dirs_data_dir),
        ipc_port: std::env::var("ZERON_IPC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(27654),
        default_harness: harness_from_env(),
    }
}

/// `ZERON_HARNESS` picks the default harness for chats without a config row.
fn harness_from_env() -> zeron_engine::HarnessId {
    match std::env::var("ZERON_HARNESS").as_deref().map(str::trim) {
        Ok("mock") => zeron_engine::HarnessId::Mock,
        _ => zeron_engine::HarnessId::Copilot,
    }
}

fn dirs_data_dir() -> std::path::PathBuf {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME not set"));
    data_dir_in(&home)
}

fn data_dir_in(home: &std::path::Path) -> std::path::PathBuf {
    let dir = home.join(".keiki");
    if !dir.exists() {
        for legacy in [".zeron", ".comet-native"] {
            let old = home.join(legacy);
            if !old.exists() {
                continue;
            }
            if let Err(error) = std::fs::rename(&old, &dir) {
                eprintln!("could not migrate data dir {}: {error}", old.display());
                return if dir.exists() { dir } else { old };
            }
            #[cfg(unix)]
            if let Err(error) = std::os::unix::fs::symlink(&dir, &old) {
                eprintln!(
                    "could not preserve legacy data paths {}: {error}",
                    old.display()
                );
                if std::fs::rename(&dir, &old).is_ok() {
                    return old;
                }
            }
            eprintln!("migrated data dir {} -> {}", old.display(), dir.display());
            break;
        }
    }
    dir
}

#[cfg(all(test, unix))]
static SUBPROCESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(test, unix))]
mod data_dir_tests {
    use super::data_dir_in;

    #[test]
    fn fresh_home_uses_keiki() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(data_dir_in(home.path()), home.path().join(".keiki"));
        assert!(!home.path().join(".zeron").exists());
    }

    #[test]
    fn migrates_legacy_data_and_preserves_absolute_paths() {
        for legacy in [".zeron", ".comet-native"] {
            let home = tempfile::tempdir().unwrap();
            let old = home.path().join(legacy);
            std::fs::create_dir_all(old.join("worktrees/repo")).unwrap();
            std::fs::write(old.join("worktrees/repo/file"), "checkout").unwrap();
            let dir = data_dir_in(home.path());
            assert_eq!(dir, home.path().join(".keiki"));
            assert_eq!(
                std::fs::read_to_string(dir.join("worktrees/repo/file")).unwrap(),
                "checkout"
            );
            assert_eq!(
                std::fs::read_to_string(old.join("worktrees/repo/file")).unwrap(),
                "checkout"
            );
            assert!(
                std::fs::symlink_metadata(&old)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(data_dir_in(home.path()), dir);
        }
    }

    #[test]
    fn existing_keiki_is_not_overwritten() {
        let home = tempfile::tempdir().unwrap();
        for name in [".keiki", ".zeron", ".comet-native"] {
            std::fs::create_dir(home.path().join(name)).unwrap();
            std::fs::write(home.path().join(name).join("data"), name).unwrap();
        }
        let dir = data_dir_in(home.path());
        assert_eq!(std::fs::read_to_string(dir.join("data")).unwrap(), ".keiki");
    }

    #[test]
    fn zeron_takes_precedence_over_comet_native() {
        let home = tempfile::tempdir().unwrap();
        for name in [".zeron", ".comet-native"] {
            std::fs::create_dir(home.path().join(name)).unwrap();
            std::fs::write(home.path().join(name).join("data"), name).unwrap();
        }
        let dir = data_dir_in(home.path());
        assert_eq!(std::fs::read_to_string(dir.join("data")).unwrap(), ".zeron");
    }

    #[test]
    fn failed_migration_preserves_legacy_data() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".zeron")).unwrap();
        std::fs::write(home.path().join(".zeron/data"), "saved").unwrap();
        std::os::unix::fs::symlink("missing/target", home.path().join(".keiki")).unwrap();
        assert_eq!(data_dir_in(home.path()), home.path().join(".zeron"));
        assert_eq!(
            std::fs::read_to_string(home.path().join(".zeron/data")).unwrap(),
            "saved"
        );
    }

    #[test]
    fn migrated_git_worktrees_keep_working() {
        let _guard = super::SUBPROCESS_LOCK.lock().unwrap();
        fn git(cwd: &std::path::Path, args: &[&str]) -> String {
            let output = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        }
        for internal_repo in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let old = home.path().join(".zeron");
            let repo = if internal_repo {
                old.join("repos/main")
            } else {
                home.path().join("external")
            };
            std::fs::create_dir_all(&repo).unwrap();
            git(&repo, &["init"]);
            git(
                &repo,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "commit",
                    "--allow-empty",
                    "-m",
                    "initial",
                ],
            );
            let worktree = old.join("worktrees/main/linked");
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "linked",
                    worktree.to_str().unwrap(),
                ],
            );
            std::fs::write(
                old.join("repos.json"),
                serde_json::to_string(&vec![repo.to_str().unwrap()]).unwrap(),
            )
            .unwrap();
            let dir = data_dir_in(home.path());
            let paths: Vec<String> =
                serde_json::from_str(&std::fs::read_to_string(dir.join("repos.json")).unwrap())
                    .unwrap();
            git(std::path::Path::new(&paths[0]), &["status", "--porcelain"]);
            assert_eq!(
                git(&worktree, &["branch", "--show-current"]).trim(),
                "linked"
            );
            assert_eq!(
                git(
                    &dir.join("worktrees/main/linked"),
                    &["branch", "--show-current"]
                )
                .trim(),
                "linked"
            );
            assert!(!git(&repo, &["worktree", "list", "--porcelain"]).contains("prunable"));
        }
    }
}

/// `{data_dir}/logs/zeron-{mode}.log`, previous launch preserved as `.old`.
/// Headed and headless are separate files so an embedded-engine app and a
/// daemon on the same machine never interleave writes.
///
/// The returned file holds an exclusive `flock` for the process lifetime:
/// rotate-on-launch is only safe when nothing is still WRITING the current
/// file. On 2026-08-04 a dev build launched twice next to the running
/// installed app — the first rename put the daemon's live log at `.old`, the
/// second unlinked it entirely, and the daemon spent the rest of the incident
/// logging to an orphaned inode (an entire day of sync diagnostics gone at
/// the exact moment they were needed). A launch that finds the canonical file
/// locked logs to `zeron-{mode}.{pid}.log` instead; the next lock-holding
/// launch sweeps pid-suffixed files older than a week.
fn open_log_file(mode: &str) -> Option<std::fs::File> {
    let dir = std::env::var_os("ZERON_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(dirs_data_dir)
        .join("logs");
    open_log_file_in(&dir, mode)
}

/// Dir-parameterized body of [`open_log_file`] (unit-testable without env).
fn open_log_file_in(dir: &std::path::Path, mode: &str) -> Option<std::fs::File> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("zeron-{mode}.log"));
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // Probe the CURRENT inode for a live writer before touching it.
        let preexisting = path.exists();
        let existing = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .ok()?;
        let rc = unsafe { libc::flock(existing.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            // A live process owns the canonical log — leave it alone.
            return std::fs::File::create(
                dir.join(format!("zeron-{mode}.{}.log", std::process::id())),
            )
            .ok();
        }
        // No live writer: rotate, create fresh, and lock it as ours. (The
        // probe's flock dies with `existing`; a first-ever launch has nothing
        // to rotate — the probe itself created the empty file.)
        drop(existing);
        if preexisting {
            let _ = std::fs::rename(&path, dir.join(format!("zeron-{mode}.log.old")));
        }
        let file = std::fs::File::create(&path).ok()?;
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        sweep_stale_pid_logs(dir, mode);
        Some(file)
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::rename(&path, dir.join(format!("zeron-{mode}.log.old")));
        std::fs::File::create(&path).ok()
    }
}

#[cfg(all(test, unix))]
mod log_file_tests {
    use super::open_log_file_in;

    #[test]
    fn second_launch_never_rotates_a_live_processes_log() {
        let _guard = super::SUBPROCESS_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        // First launch owns the canonical file and keeps writing.
        let first = open_log_file_in(dir, "headed").expect("first log");
        assert!(dir.join("zeron-headed.log").is_file());
        // Second launch while the first is alive: canonical file untouched,
        // pid-suffixed overflow file instead (the 2026-08-04 clobber).
        let second = open_log_file_in(dir, "headed").expect("second log");
        let pid_path = dir.join(format!("zeron-headed.{}.log", std::process::id()));
        assert!(pid_path.is_file(), "expected pid-suffixed overflow log");
        assert!(
            !dir.join("zeron-headed.log.old").exists(),
            "live canonical log must not be rotated away"
        );
        drop(second);
        // After the owner exits, a fresh launch rotates normally.
        drop(first);
        let third = open_log_file_in(dir, "headed").expect("third log");
        assert!(
            dir.join("zeron-headed.log.old").is_file(),
            "rotation resumes"
        );
        drop(third);
    }
}

/// Delete `zeron-{mode}.{pid}.log` overflow files older than a week — they
/// only exist when a second instance raced a live one for the canonical log.
#[cfg(unix)]
fn sweep_stale_pid_logs(dir: &std::path::Path, mode: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("zeron-{mode}.");
    let week = std::time::Duration::from_secs(7 * 24 * 60 * 60);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(middle) = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".log"))
        else {
            continue;
        };
        if !middle.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > week);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
