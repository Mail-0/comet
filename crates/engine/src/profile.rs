//! Workspace profile identity and storage boundaries.
//!
//! Stores, journals, and uploads are profile-scoped. Repositories, worktrees,
//! agent credentials, UI settings, and the device id remain device-scoped under
//! the engine data directory.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeron_proto::WorkspaceScope;

use crate::EngineError;

const LOCAL_PROFILE_FILE: &str = "local-profile.json";
const LOCAL_ORG_ID: &str = "local";

/// A resolved, immutable identity and storage boundary for one engine runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineProfile {
    device_root: PathBuf,
    store_root: PathBuf,
    uploads_root: PathBuf,
    org_id: String,
    user_id: String,
}

impl EngineProfile {
    /// Load or create the installation's stable local identity.
    pub fn local(data_dir: &Path) -> Result<Self, EngineError> {
        let profile_id = load_or_create_local_profile_id(data_dir)?;
        let store_root = data_dir.join("profiles").join("local");
        Ok(Self {
            device_root: data_dir.to_path_buf(),
            uploads_root: store_root.join("uploads"),
            store_root,
            org_id: LOCAL_ORG_ID.to_string(),
            user_id: profile_id,
        })
    }

    pub fn scope(&self) -> WorkspaceScope {
        WorkspaceScope::Local
    }

    pub fn device_root(&self) -> &Path {
        &self.device_root
    }

    pub fn store_root(&self) -> &Path {
        &self.store_root
    }

    pub fn uploads_root(&self) -> &Path {
        &self.uploads_root
    }

    pub fn org_id(&self) -> &str {
        &self.org_id
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalProfileFile {
    id: Uuid,
}



fn load_or_create_local_profile_id(data_dir: &Path) -> Result<String, EngineError> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join(LOCAL_PROFILE_FILE);
    match read_local_profile_id(&path) {
        Ok(id) => return Ok(id),
        Err(ProfileReadError::Missing) => {}
        Err(ProfileReadError::Engine(err)) => return Err(err),
    }

    let id = Uuid::new_v4();
    let mut bytes = serde_json::to_vec_pretty(&LocalProfileFile { id })
        .map_err(|err| EngineError::Other(format!("serialize local profile: {err}")))?;
    bytes.push(b'\n');

    // Publish a fully-written same-directory file without replacing a profile
    // another process may have created concurrently.
    let temp_path = data_dir.join(format!(
        ".{LOCAL_PROFILE_FILE}.tmp-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let write_result = (|| -> Result<(), EngineError> {
        let mut temp = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        temp.write_all(&bytes)?;
        temp.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }

    let publish_result = std::fs::hard_link(&temp_path, &path);
    let _ = std::fs::remove_file(&temp_path);
    match publish_result {
        Ok(()) => Ok(id.to_string()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            read_local_profile_id(&path).map_err(ProfileReadError::into_engine)
        }
        Err(err) => Err(err.into()),
    }
}

enum ProfileReadError {
    Missing,
    Engine(EngineError),
}

impl ProfileReadError {
    fn into_engine(self) -> EngineError {
        match self {
            Self::Missing => EngineError::Other("local profile disappeared during creation".into()),
            Self::Engine(err) => err,
        }
    }
}

fn read_local_profile_id(path: &Path) -> Result<String, ProfileReadError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProfileReadError::Missing);
        }
        Err(err) => return Err(ProfileReadError::Engine(err.into())),
    };
    let profile: LocalProfileFile = serde_json::from_slice(&bytes).map_err(|err| {
        ProfileReadError::Engine(EngineError::Other(format!(
            "invalid local profile {}: {err}",
            path.display()
        )))
    })?;
    if profile.id.is_nil() {
        return Err(ProfileReadError::Engine(EngineError::Other(format!(
            "invalid local profile {}: id must not be nil",
            path.display()
        ))));
    }
    Ok(profile.id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_profile_has_a_stable_identity_and_storage_root() {
        let dir = tempfile::tempdir().unwrap();
        let first = EngineProfile::local(dir.path()).unwrap();
        let again = EngineProfile::local(dir.path()).unwrap();
        assert_eq!(first, again);
        assert_eq!(first.scope(), WorkspaceScope::Local);
        assert_eq!(first.store_root(), dir.path().join("profiles/local"));
        assert_eq!(first.uploads_root(), dir.path().join("profiles/local/uploads"));
        assert!(Uuid::parse_str(first.user_id()).is_ok());
    }

    #[test]
    fn a_broken_local_identity_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCAL_PROFILE_FILE);
        std::fs::write(&path, b"not json").unwrap();
        assert!(EngineProfile::local(dir.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"not json");
    }

    #[test]
    fn local_storage_never_claims_or_changes_account_archives() {
        let dir = tempfile::tempdir().unwrap();
        let archived = dir.path().join("orgs/old-org/old-user");
        std::fs::create_dir_all(archived.join("uploads")).unwrap();
        std::fs::create_dir_all(dir.path().join("uploads")).unwrap();
        let sentinels = [
            archived.join("docs.sqlite3"), archived.join("uploads/kept"),
            dir.path().join("uploads/kept"), dir.path().join("legacy-uploads-owner.json"),
        ];
        for path in &sentinels { std::fs::write(path, b"untouched archive").unwrap(); }
        let local = EngineProfile::local(dir.path()).unwrap();
        let _store = zeron_sync::DocsStore::open(local.store_root()).unwrap();
        for path in &sentinels { assert_eq!(std::fs::read(path).unwrap(), b"untouched archive"); }
        assert!(!archived.join("local-profile.json").exists());
    }
}
