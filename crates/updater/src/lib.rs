//! zeron-updater — self-update against the GitHub Release the `release`
//! workflow publishes for every `v<version>` tag (`.github/workflows/release.yml`).
//!
//! Flow, driven by `zeron-ui`'s `updater` module:
//! 1. [`Updater::check`] reads the repo's latest Release, compares its tag to
//!    the running [`running_version`], and resolves the artifact matching this
//!    installation ([`InstallTarget`]) — a tarball binary swap, an AppImage
//!    file swap, or a macOS bundle replacement from the dmg;
//! 2. [`Updater::download`] streams that asset to a temp file, reporting bytes
//!    for the update modal's progress;
//! 3. [`InstallTarget::install`] swaps the new build in place, and
//!    [`queue_relaunch`] + [`run_pending_relaunch`] restart the app once the
//!    window loop has exited.
//!
//! Nothing here is macOS/Linux-specific at the API level: an installation the
//! updater cannot replace (a `.deb` under `/usr/bin`, a `cargo run` build)
//! reports [`Error::Unsupported`] from the detect/writability probes, and the
//! UI stays silent instead of looping on a download it cannot apply.

mod install;

pub use install::{InstallTarget, queue_relaunch, run_pending_relaunch};

use std::path::{Path, PathBuf};

use futures::StreamExt as _;
use serde::Deserialize;

/// Latest Release of the repository the binary is built from.
pub const DEFAULT_FEED_URL: &str = "https://api.github.com/repos/Mail-0/comet/releases/latest";

/// Version of the running build — the workspace version every crate shares,
/// which the release workflow's version guard pins to the tag.
pub fn running_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("update feed request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("update feed returned {status}")]
    Status { status: reqwest::StatusCode },
    #[error("{0}")]
    Unsupported(String),
    #[error("release {tag} has no {asset} artifact")]
    MissingAsset { tag: String, asset: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Install(String),
}

/// One published Release, trimmed to what the updater needs.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// A Release artifact. `url` is the API asset endpoint (not
/// `browser_download_url`) so the same request works for a private repo with a
/// token as it does anonymously for a public one.
#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub size: u64,
}

/// A newer release that this installation can actually apply.
#[derive(Debug, Clone)]
pub struct Available {
    /// Tag with the leading `v` stripped — what the UI shows.
    pub version: String,
    pub tag: String,
    pub asset: Asset,
    pub target: InstallTarget,
}

#[derive(Debug, Clone)]
pub struct Updater {
    http: reqwest::Client,
    feed_url: String,
    /// Bearer token for the Release API. Only needed while the repo is
    /// private; a public Release downloads anonymously.
    token: Option<String>,
}

impl Updater {
    /// `ZERON_UPDATE_FEED` overrides the feed (test releases), and
    /// `ZERON_UPDATE_TOKEN`/`GITHUB_TOKEN` authenticates it.
    pub fn from_env() -> Result<Self, Error> {
        let feed_url = std::env::var("ZERON_UPDATE_FEED")
            .ok()
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| DEFAULT_FEED_URL.to_string());
        let token = ["ZERON_UPDATE_TOKEN", "GITHUB_TOKEN"]
            .iter()
            .filter_map(|key| std::env::var(key).ok())
            .map(|token| token.trim().to_string())
            .find(|token| !token.is_empty());
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("zeron/", env!("CARGO_PKG_VERSION")))
                .build()?,
            feed_url,
            token,
        })
    }

    /// The repo's latest Release.
    pub async fn latest(&self) -> Result<Release, Error> {
        let response = self
            .request(&self.feed_url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Status { status });
        }
        Ok(response.json::<Release>().await?)
    }

    /// `Some(update)` when the latest Release is newer than `current` AND ships
    /// an artifact for this installation. Detect/writability failures surface as
    /// [`Error::Unsupported`] so the caller can log once and stop checking.
    pub async fn check(&self, current: &str) -> Result<Option<Available>, Error> {
        let target = InstallTarget::detect()?;
        let release = self.latest().await?;
        let version = release.tag_name.trim().trim_start_matches('v').to_string();
        if !is_newer(current, &version) {
            return Ok(None);
        }
        let wanted = target.asset_name(&version);
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == wanted)
            .cloned()
            .ok_or_else(|| Error::MissingAsset {
                tag: release.tag_name.clone(),
                asset: wanted,
            })?;
        target.ensure_writable()?;
        Ok(Some(Available {
            version,
            tag: release.tag_name,
            asset,
            target,
        }))
    }

    /// Stream `asset` into `dir`, calling `progress(downloaded, total)` as bytes
    /// land (`total` is `None` when the response has no length).
    pub async fn download(
        &self,
        asset: &Asset,
        dir: &Path,
        mut progress: impl FnMut(u64, Option<u64>),
    ) -> Result<PathBuf, Error> {
        let response = self
            .request(&asset.url)
            .header(reqwest::header::ACCEPT, "application/octet-stream")
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Status { status });
        }
        let total = response
            .content_length()
            .or(Some(asset.size).filter(|size| *size > 0));
        let path = dir.join(&asset.name);
        let mut file = std::fs::File::create(&path)?;
        let mut downloaded = 0u64;
        progress(0, total);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            std::io::Write::write_all(&mut file, &chunk)?;
            downloaded += chunk.len() as u64;
            progress(downloaded, total);
        }
        std::io::Write::flush(&mut file)?;
        Ok(path)
    }

    fn request(&self, url: &str) -> reqwest::RequestBuilder {
        let request = self.http.get(url);
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }
}

/// `true` when `candidate` is a later version than `current`. Both may carry a
/// leading `v` and a pre-release/build suffix; a suffix loses a tie against the
/// same release triple (`0.4.0-rc.1` < `0.4.0`), so a nightly tester lands on
/// the final build.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    match (parse_version(current), parse_version(candidate)) {
        (Some(current), Some(candidate)) => candidate > current,
        _ => false,
    }
}

/// `(major, minor, patch, is_release)` — `is_release` is false for a
/// pre-release suffix, which sorts BELOW the same triple's final build.
fn parse_version(version: &str) -> Option<(u64, u64, u64, bool)> {
    let version = version.trim().trim_start_matches('v');
    let core = version
        .split_once(['-', '+'])
        .map_or(version, |(core, _)| core);
    let release = core.len() == version.len();
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    parts
        .next()
        .is_none()
        .then_some((major, minor, patch, release))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch_minor_and_major_all_update() {
        assert!(is_newer("0.3.1", "0.3.2"));
        assert!(is_newer("0.3.1", "0.4.0"));
        assert!(is_newer("0.3.1", "1.0.0"));
        assert!(is_newer("0.3.1", "v0.3.2"));
    }

    #[test]
    fn same_or_older_release_never_updates() {
        assert!(!is_newer("0.3.1", "0.3.1"));
        assert!(!is_newer("0.3.1", "0.3.0"));
        assert!(!is_newer("0.10.0", "0.9.9"));
        // A dev build ahead of the published Release stays put.
        assert!(!is_newer("0.4.0", "0.3.9"));
    }

    #[test]
    fn prerelease_loses_ties_to_the_final_build() {
        assert!(is_newer("0.4.0-rc.1", "0.4.0"));
        assert!(!is_newer("0.4.0", "0.4.0-rc.2"));
        assert!(is_newer("0.3.1", "0.4.0-rc.1"));
    }

    #[test]
    fn unparseable_tags_are_not_updates() {
        // A malformed feed must never trigger a download.
        assert!(!is_newer("0.3.1", "nightly"));
        assert!(!is_newer("0.3.1", ""));
        assert!(!is_newer("0.3.1", "0.3.2.1"));
    }

    #[test]
    fn release_json_parses_the_fields_the_updater_uses() {
        let release: Release = serde_json::from_str(
            r#"{
                "tag_name": "v0.4.0",
                "name": "0.4.0",
                "assets": [{
                    "name": "zeron-0.4.0-linux-x86_64.tar.gz",
                    "url": "https://api.github.com/repos/Mail-0/comet/releases/assets/1",
                    "browser_download_url": "https://example.invalid/dl",
                    "size": 42
                }]
            }"#,
        )
        .expect("release json");
        assert_eq!(release.tag_name, "v0.4.0");
        assert_eq!(release.assets.len(), 1);
        assert_eq!(release.assets[0].size, 42);
    }
}
