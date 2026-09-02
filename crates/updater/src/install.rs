//! Where this build lives on disk, how a downloaded artifact replaces it, and
//! how the app comes back afterwards.
//!
//! The three shapes the release workflow produces map to three swaps:
//! - [`InstallTarget::AppImage`] — the running `.AppImage` file itself
//!   (`$APPIMAGE`, exported by the runtime), replaced wholesale;
//! - [`InstallTarget::LinuxBinary`] — an installed `zeron` executable (the
//!   tarball's `install.sh` puts it in `~/.local/bin`), replaced with the
//!   binary out of the new tarball;
//! - [`InstallTarget::MacBundle`] — `Keiki.app`, replaced with the bundle
//!   inside the new dmg.
//!
//! Every swap is a rename inside the target's own directory, so a failed
//! download or a killed process can never leave a half-written executable
//! where the old working one was.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallTarget {
    AppImage { path: PathBuf },
    LinuxBinary { path: PathBuf },
    MacBundle { bundle: PathBuf, binary: PathBuf },
}

impl InstallTarget {
    /// Classify the running process. [`Error::Unsupported`] for anything the
    /// updater must not touch — a `cargo` build tree, or an executable owned by
    /// a package manager.
    pub fn detect() -> Result<Self, Error> {
        let exe = std::env::current_exe()?;
        let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
        Self::classify(&exe, appimage.as_deref())
    }

    /// Path-only body of [`detect`] (unit-testable without a real install).
    pub fn classify(exe: &Path, appimage: Option<&Path>) -> Result<Self, Error> {
        if let Some(appimage) = appimage {
            return Ok(Self::AppImage {
                path: appimage.to_path_buf(),
            });
        }
        if let Some(bundle) = mac_bundle(exe) {
            return Ok(Self::MacBundle {
                bundle,
                binary: exe.to_path_buf(),
            });
        }
        if is_build_tree(exe) {
            return Err(Error::Unsupported(format!(
                "{} is a development build",
                exe.display()
            )));
        }
        Ok(Self::LinuxBinary {
            path: exe.to_path_buf(),
        })
    }

    /// Release asset that carries `version` for this installation
    /// (`scripts/package-{linux,macos}.sh` name their outputs from the same
    /// `<version>`/`<arch>` pair).
    pub fn asset_name(&self, version: &str) -> String {
        match self {
            Self::AppImage { .. } => format!("zeron-{version}-linux-{}.AppImage", asset_arch()),
            Self::LinuxBinary { .. } => format!("zeron-{version}-linux-{}.tar.gz", asset_arch()),
            Self::MacBundle { .. } => format!("zeron-{version}-macos-{}.dmg", asset_arch()),
        }
    }

    /// The file or bundle the swap replaces.
    fn swap_path(&self) -> &Path {
        match self {
            Self::AppImage { path } | Self::LinuxBinary { path } => path,
            Self::MacBundle { bundle, .. } => bundle,
        }
    }

    /// Fail BEFORE downloading when the swap could not land: a `.deb` install
    /// under `/usr/bin` or a `/Applications` bundle owned by another user needs
    /// privileges the app does not have, and the user updates it the way they
    /// installed it.
    pub fn ensure_writable(&self) -> Result<(), Error> {
        let path = self.swap_path();
        let parent = path
            .parent()
            .ok_or_else(|| Error::Unsupported(format!("{} has no parent", path.display())))?;
        let probe = parent.join(format!(".zeron-update-probe-{}", std::process::id()));
        match std::fs::File::create(&probe) {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                Ok(())
            }
            Err(error) => Err(Error::Unsupported(format!(
                "{} is not writable ({error}) — update through the package manager that installed it",
                parent.display()
            ))),
        }
    }

    /// Unpack `downloaded` and move it over the running installation.
    pub fn install(&self, downloaded: &Path) -> Result<(), Error> {
        match self {
            Self::AppImage { path } => swap_in(path, downloaded, true),
            Self::LinuxBinary { path } => {
                let staging = tempfile::tempdir_in(
                    path.parent()
                        .ok_or_else(|| Error::Install("binary has no parent".into()))?,
                )?;
                run(
                    "tar",
                    &[
                        "-xzf".as_ref(),
                        downloaded.as_os_str(),
                        "-C".as_ref(),
                        staging.path().as_os_str(),
                    ],
                )?;
                let binary = find_file(staging.path(), "zeron", 2).ok_or_else(|| {
                    Error::Install(format!("no zeron binary in {}", downloaded.display()))
                })?;
                swap_in(path, &binary, true)
            }
            Self::MacBundle { bundle, .. } => {
                let mount = tempfile::tempdir()?;
                run(
                    "hdiutil",
                    &[
                        "attach".as_ref(),
                        downloaded.as_os_str(),
                        "-nobrowse".as_ref(),
                        "-readonly".as_ref(),
                        "-mountpoint".as_ref(),
                        mount.path().as_os_str(),
                    ],
                )?;
                let result = install_from_dmg(bundle, mount.path());
                // Always detach: a mounted image left behind blocks the next
                // update's attach on the same volume name.
                let _ = run(
                    "hdiutil",
                    &[
                        "detach".as_ref(),
                        mount.path().as_os_str(),
                        "-quiet".as_ref(),
                    ],
                );
                result
            }
        }
    }

    /// Command that starts the freshly installed build.
    fn relaunch(&self) -> Command {
        match self {
            Self::AppImage { path } | Self::LinuxBinary { path } => Command::new(path),
            Self::MacBundle { bundle, .. } => {
                let mut command = Command::new("open");
                command.arg("-n").arg(bundle);
                command
            }
        }
    }
}

/// Copy the new bundle out of the mounted dmg and swap it in.
fn install_from_dmg(bundle: &Path, mount: &Path) -> Result<(), Error> {
    let source = std::fs::read_dir(mount)?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "app"))
        .ok_or_else(|| Error::Install("dmg contains no .app bundle".into()))?;
    let parent = bundle
        .parent()
        .ok_or_else(|| Error::Install("bundle has no parent".into()))?;
    let staging = tempfile::tempdir_in(parent)?;
    let staged = staging.path().join(
        source
            .file_name()
            .ok_or_else(|| Error::Install("bundle has no name".into()))?,
    );
    // `ditto` (not fs::copy) preserves the bundle's symlinks, resource forks,
    // and — critically — its code signature.
    run("ditto", &[source.as_os_str(), staged.as_os_str()])?;
    swap_in(bundle, &staged, false)
}

/// Replace `target` with `replacement` through a same-directory rename, keeping
/// the previous copy until the new one is in place.
fn swap_in(target: &Path, replacement: &Path, executable: bool) -> Result<(), Error> {
    let parent = target
        .parent()
        .ok_or_else(|| Error::Install(format!("{} has no parent", target.display())))?;
    let incoming = parent.join(format!(
        ".{}.new-{}",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("zeron"),
        std::process::id()
    ));
    let outgoing = parent.join(format!(
        ".{}.old-{}",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("zeron"),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&incoming);
    let _ = std::fs::remove_dir_all(&incoming);
    // `rename` across filesystems fails (the download lives in a temp dir), so
    // land the payload beside the target first.
    if replacement.is_dir() {
        run(
            "cp",
            &["-R".as_ref(), replacement.as_os_str(), incoming.as_os_str()],
        )?;
    } else {
        std::fs::copy(replacement, &incoming)?;
    }
    if executable {
        set_executable(&incoming)?;
    }
    // The running binary's inode stays alive while the process holds it open;
    // moving it aside (rather than unlinking) also leaves a recoverable copy if
    // the second rename fails.
    let had_target = target.exists();
    if had_target {
        std::fs::rename(target, &outgoing)?;
    }
    match std::fs::rename(&incoming, target) {
        Ok(()) => {
            if outgoing.is_dir() {
                let _ = std::fs::remove_dir_all(&outgoing);
            } else {
                let _ = std::fs::remove_file(&outgoing);
            }
            Ok(())
        }
        Err(error) => {
            // Put the working install back before reporting the failure.
            if had_target {
                let _ = std::fs::rename(&outgoing, target);
            }
            Err(Error::Io(error))
        }
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), Error> {
    Ok(())
}

fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<(), Error> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| Error::Install(format!("{program} failed to start: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::Install(format!(
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// First `name` at most `depth` levels under `dir` (the tarball nests the
/// binary one directory deep).
fn find_file(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut nested = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            nested.push(path);
        } else if path.file_name().is_some_and(|file| file == name) {
            return Some(path);
        }
    }
    if depth == 0 {
        return None;
    }
    nested
        .into_iter()
        .find_map(|dir| find_file(&dir, name, depth - 1))
}

/// `…/Keiki.app` for an executable inside a macOS bundle.
fn mac_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    (macos.file_name()? == "MacOS")
        .then(|| macos.parent())
        .flatten()
        .filter(|contents| contents.file_name().is_some_and(|name| name == "Contents"))
        .and_then(Path::parent)
        .filter(|bundle| bundle.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
}

/// `cargo build` output — never self-update over a dev tree.
fn is_build_tree(exe: &Path) -> bool {
    let mut components = exe.components().rev().skip(1);
    let profile = components.next();
    let target = components.next();
    match (profile, target) {
        (Some(profile), Some(target)) => {
            target.as_os_str() == "target"
                && matches!(profile.as_os_str().to_str(), Some("debug" | "release"))
        }
        _ => false,
    }
}

/// Architecture slug the packaging scripts use — `uname -m`, which reports
/// `arm64` on Apple silicon where Rust says `aarch64`.
fn asset_arch() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, arch) => arch,
    }
}

/// Restart queued by the UI. The window loop must exit first: the engine
/// flushes its stores and releases the single-instance lock on quit, and the
/// new process takes both.
static PENDING_RELAUNCH: Mutex<Option<InstallTarget>> = Mutex::new(None);

pub fn queue_relaunch(target: InstallTarget) {
    if let Ok(mut pending) = PENDING_RELAUNCH.lock() {
        *pending = Some(target);
    }
}

/// Start the installed build, if an update queued a restart. Called by the
/// binary right after the gpui application loop returns — the old process then
/// exits normally, dropping its locks.
pub fn run_pending_relaunch() {
    let Some(target) = PENDING_RELAUNCH.lock().ok().and_then(|mut p| p.take()) else {
        return;
    };
    match target.relaunch().spawn() {
        Ok(child) => tracing::info!(pid = child.id(), "relaunched after update"),
        Err(error) => tracing::error!(%error, "relaunch after update failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appimage_wins_over_the_extracted_binary_path() {
        // Inside an AppImage, current_exe() points at the mount, which
        // disappears on exit — $APPIMAGE is the file to replace.
        let target = InstallTarget::classify(
            Path::new("/tmp/.mount_zeronAbc/usr/bin/zeron"),
            Some(Path::new("/home/ada/Apps/Keiki.AppImage")),
        )
        .expect("appimage target");
        assert_eq!(
            target,
            InstallTarget::AppImage {
                path: PathBuf::from("/home/ada/Apps/Keiki.AppImage")
            }
        );
        assert_eq!(
            target.asset_name("0.4.0"),
            format!("zeron-0.4.0-linux-{}.AppImage", asset_arch())
        );
    }

    #[test]
    fn installed_binary_maps_to_the_tarball() {
        let target =
            InstallTarget::classify(Path::new("/home/ada/.local/bin/zeron"), None).expect("binary");
        assert_eq!(
            target,
            InstallTarget::LinuxBinary {
                path: PathBuf::from("/home/ada/.local/bin/zeron")
            }
        );
        assert_eq!(
            target.asset_name("0.4.0"),
            format!("zeron-0.4.0-linux-{}.tar.gz", asset_arch())
        );
    }

    #[test]
    fn mac_bundle_maps_to_the_dmg() {
        let target = InstallTarget::classify(
            Path::new("/Applications/Keiki.app/Contents/MacOS/zeron"),
            None,
        )
        .expect("bundle");
        assert_eq!(
            target,
            InstallTarget::MacBundle {
                bundle: PathBuf::from("/Applications/Keiki.app"),
                binary: PathBuf::from("/Applications/Keiki.app/Contents/MacOS/zeron"),
            }
        );
        assert_eq!(
            target.asset_name("0.4.0"),
            format!("zeron-0.4.0-macos-{}.dmg", asset_arch())
        );
    }

    #[test]
    fn cargo_builds_are_never_self_updated() {
        for path in [
            "/home/ada/repos/comet/target/debug/zeron",
            "/home/ada/repos/comet/target/release/zeron",
        ] {
            assert!(matches!(
                InstallTarget::classify(Path::new(path), None),
                Err(Error::Unsupported(_))
            ));
        }
        // A tarball install one directory deep is not a build tree.
        assert!(InstallTarget::classify(Path::new("/opt/zeron/zeron"), None).is_ok());
    }

    #[test]
    fn read_only_install_dirs_fail_before_the_download() {
        let target = InstallTarget::LinuxBinary {
            path: PathBuf::from("/proc/1/zeron"),
        };
        assert!(matches!(
            target.ensure_writable(),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn swap_keeps_the_old_install_until_the_new_one_lands() {
        let dir = tempfile::tempdir().expect("dir");
        let target = dir.path().join("zeron");
        std::fs::write(&target, b"old").expect("old binary");
        // The download lands in a separate temp dir, as it does in the app.
        let download = tempfile::tempdir().expect("download dir");
        let replacement = download.path().join("zeron");
        std::fs::write(&replacement, b"new").expect("new binary");

        swap_in(&target, &replacement, true).expect("swap");

        assert_eq!(std::fs::read(&target).expect("swapped"), b"new");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .map(|entry| entry.file_name())
            .filter(|name| name != "zeron")
            .collect();
        assert!(leftovers.is_empty(), "swap left {leftovers:?} behind");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&target)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "swapped binary must stay executable");
        }
    }

    #[test]
    fn swap_installs_a_first_time_target() {
        let dir = tempfile::tempdir().expect("dir");
        let target = dir.path().join("zeron");
        let replacement = dir.path().join("payload");
        std::fs::write(&replacement, b"new").expect("payload");

        swap_in(&target, &replacement, true).expect("swap");

        assert_eq!(std::fs::read(&target).expect("installed"), b"new");
    }

    #[test]
    fn tarball_binary_is_found_one_directory_deep() {
        let dir = tempfile::tempdir().expect("dir");
        let nested = dir.path().join("zeron-0.4.0-linux-x86_64");
        std::fs::create_dir(&nested).expect("nested");
        std::fs::write(nested.join("zeron.desktop"), b"").expect("desktop");
        std::fs::write(nested.join("zeron"), b"").expect("binary");

        assert_eq!(
            find_file(dir.path(), "zeron", 2),
            Some(nested.join("zeron"))
        );
        assert_eq!(find_file(dir.path(), "missing", 2), None);
    }
}
