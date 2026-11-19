//! Application storage paths. Provider credentials keep their own locations.

use std::ffi::OsString;
use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    resolve_data_dir(|name| std::env::var_os(name))
}

/// [`data_dir`] from `env` (`ZERON_DATA_DIR`, `HOME`, and on Windows
/// `LOCALAPPDATA`/`USERPROFILE`): for an app resolving it as Zeron does.
pub fn resolve_data_dir(mut env: impl FnMut(&str) -> Option<OsString>) -> PathBuf {
    if let Some(dir) = env("ZERON_DATA_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    {
        // Explorer does not set HOME. Do not let a shell-specific HOME select
        // a different workspace from a desktop launch, or migrate credentials
        // between Unix-style and native Windows directories implicitly.
        let local = env("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env("USERPROFILE")
                    .filter(|value| !value.is_empty())
                    .map(|home| PathBuf::from(home).join("AppData").join("Local"))
            })
            .expect("LOCALAPPDATA and USERPROFILE not set; set ZERON_DATA_DIR");
        local.join("Zeron")
    }
    #[cfg(not(windows))]
    {
        let home = PathBuf::from(env("HOME").expect("HOME not set"));
        data_dir_in(&home)
    }
}

fn data_dir_in(home: &std::path::Path) -> PathBuf {
    let dir = home.join(".keiki");
    if !dir.exists() {
        for legacy in [".zeron", ".comet-native"] {
            let old = home.join(legacy);
            if !old.exists() {
                continue;
            }
            let has_account_data = ["orgs", "uploads", "legacy-uploads-owner.json"]
                .iter()
                .any(|entry| {
                    !matches!(
                        std::fs::symlink_metadata(old.join(entry)),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound
                    )
                });
            if has_account_data {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(vars: &[(&str, &str)]) -> PathBuf {
        resolve_data_dir(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.into())
        })
    }

    #[test]
    fn explicit_data_dir_needs_no_home() {
        assert_eq!(
            resolve(&[("ZERON_DATA_DIR", "custom data")]),
            PathBuf::from("custom data")
        );
    }

    #[cfg(windows)]
    #[test]
    fn explorer_launch_without_home_uses_local_app_data() {
        assert_eq!(
            resolve(&[("LOCALAPPDATA", r"C:\Users\Test User\AppData\Local")]),
            PathBuf::from(r"C:\Users\Test User\AppData\Local\Zeron"),
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_profile_fallback_handles_unicode_and_apostrophes() {
        assert_eq!(
            resolve(&[("USERPROFILE", r"C:\Users\O'Brien 日本語")]),
            PathBuf::from(r"C:\Users\O'Brien 日本語\AppData\Local\Zeron"),
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_default_does_not_depend_on_shell_home() {
        assert_eq!(
            resolve(&[("HOME", r"D:\msys-home"), ("LOCALAPPDATA", r"C:\Local")]),
            PathBuf::from(r"C:\Local\Zeron"),
        );
    }
}

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
    fn account_stores_and_legacy_uploads_are_not_migration_sources() {
        for legacy in [".zeron", ".comet-native"] {
            for entry in ["orgs", "uploads", "legacy-uploads-owner.json"] {
                let home = tempfile::tempdir().unwrap();
                let old = home.path().join(legacy);
                std::fs::create_dir(&old).unwrap();
                let sentinel = if entry.ends_with(".json") {
                    old.join(entry)
                } else {
                    std::fs::create_dir_all(old.join(entry).join("old-org/old-user")).unwrap();
                    old.join(entry).join("old-org/old-user/kept")
                };
                std::fs::write(&sentinel, b"untouched account data").unwrap();
                assert_eq!(data_dir_in(home.path()), home.path().join(".keiki"));
                assert!(!home.path().join(".keiki").exists());
                assert!(!std::fs::symlink_metadata(&old).unwrap().file_type().is_symlink());
                assert_eq!(std::fs::read(&sentinel).unwrap(), b"untouched account data");
            }
        }
    }

    #[test]
    fn a_local_only_legacy_store_can_migrate_without_moving_a_zeron_account() {
        let home = tempfile::tempdir().unwrap();
        let archived = home.path().join(".zeron/orgs/old-org/old-user");
        std::fs::create_dir_all(&archived).unwrap();
        std::fs::write(archived.join("kept"), b"untouched").unwrap();
        let local = home.path().join(".comet-native/profiles/local");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("data"), b"local").unwrap();
        let dir = data_dir_in(home.path());
        assert_eq!(std::fs::read(dir.join("profiles/local/data")).unwrap(), b"local");
        assert_eq!(std::fs::read(archived.join("kept")).unwrap(), b"untouched");
        assert!(!std::fs::symlink_metadata(home.path().join(".zeron")).unwrap().file_type().is_symlink());
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
