//! Local engine profile and storage boundaries.
//!
//! All durable engine state is local to this installation. Account-scoped
//! profile roots from older releases are intentionally no longer reachable.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::EngineError;

const LOCAL_PROFILE_FILE: &str = "local-profile.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineProfile {
    device_root: PathBuf,
    store_root: PathBuf,
    uploads_root: PathBuf,
}

impl EngineProfile {
    pub fn local(data_dir: &Path) -> Result<Self, EngineError> {
        let _ = load_or_create_local_profile_id(data_dir)?;
        let store_root = data_dir.join("profiles").join("local");
        Ok(Self {
            device_root: data_dir.to_path_buf(),
            uploads_root: store_root.join("uploads"),
            store_root,
        })
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
