use std::collections::BTreeMap;

use anyhow::{Context as _, ensure};
use serde::Deserialize;

use crate::{Channel, FileMeta, Manifest};

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    url: String,
    #[serde(default)]
    digest: Option<String>,
}

pub(crate) fn is_feed(url: &str) -> bool {
    url.trim_end_matches('/').ends_with("/releases/latest") || url.contains("/releases/tags/")
}

pub(crate) fn request(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
) -> reqwest::RequestBuilder {
    let token = ["KEIKI_UPDATE_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.trim().is_empty());
    request_with_token(client, method, url, token.as_deref())
}

fn request_with_token(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut request = client
        .request(method, url)
        .header(reqwest::header::ACCEPT, "application/octet-stream");
    if reqwest::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str() == Some("api.github.com")
            && u.username().is_empty()
            && u.password().is_none()
    }) {
        if let Some(token) = token {
            request = request.bearer_auth(token.trim());
        }
    }
    request
}

pub(crate) async fn fetch(feed: &str, channel: Channel) -> anyhow::Result<Manifest> {
    let client = crate::http_client()?;
    let listing = channel == Channel::Beta && feed.trim_end_matches('/').ends_with("/latest");
    let url = if listing {
        format!(
            "{}?per_page=20",
            feed.trim_end_matches('/').trim_end_matches("/latest")
        )
    } else {
        feed.to_owned()
    };
    let response = crate::fetch_release_metadata(&client, &url).await?;
    ensure!(
        response.status.is_success(),
        "release feed returned {}",
        response.status
    );
    let body = response.body;
    let release: Release = if listing {
        serde_json::from_slice::<Vec<Release>>(&body)?
            .into_iter()
            .find(|release| !release.draft)
            .context("no published Keiki release")?
    } else {
        serde_json::from_slice(&body)?
    };
    ensure!(!release.draft, "release is still a draft");
    let version = release
        .tag_name
        .trim()
        .trim_start_matches("beta-")
        .trim_start_matches('v');
    ensure!(
        crate::parse_version(version).is_some(),
        "release has no valid version"
    );
    let artifact = match std::env::consts::OS {
        "macos" => crate::mac_app_artifact(version),
        "windows" => format!("keiki-{version}-windows-{}.zip", crate::platform_key().1),
        _ => crate::headless_artifact(version),
    };
    ensure!(
        release.assets.iter().any(|asset| asset.name == artifact),
        "release has no {artifact}"
    );
    let mut manifest =
        if let Some(asset) = release.assets.iter().find(|a| a.name == "manifest.json") {
            let response = crate::fetch_release_metadata(&client, &asset.url).await?;
            ensure!(
                response.status.is_success(),
                "release manifest returned {}",
                response.status
            );
            let manifest: Manifest = serde_json::from_slice(&response.body)?;
            ensure!(
                manifest.version == version,
                "manifest version does not match the release tag"
            );
            manifest
        } else {
            Manifest {
                version: version.into(),
                ..Manifest::default()
            }
        };
    manifest.channel = channel;
    manifest.asset_urls = release
        .assets
        .iter()
        .map(|a| (a.name.clone(), a.url.clone()))
        .collect::<BTreeMap<_, _>>();
    for asset in release.assets {
        if let Some(digest) = asset
            .digest
            .and_then(|d| d.strip_prefix("sha256:").map(str::to_owned))
        {
            manifest.files.entry(asset.name).or_insert(FileMeta {
                sha256: Some(digest),
            });
        }
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest as _, Sha256};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    const PACKAGE: &[u8] = b"Keiki package fixture";

    fn artifact() -> String {
        match std::env::consts::OS {
            "macos" => crate::mac_app_artifact("9.9.9"),
            "windows" => format!("keiki-9.9.9-windows-{}.zip", crate::platform_key().1),
            _ => crate::headless_artifact("9.9.9"),
        }
    }

    async fn server(mode: &str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let digest = format!("{:x}", Sha256::digest(PACKAGE));
        let mut assets = vec![serde_json::json!({
            "name": artifact(), "url": format!("{base}/assets/package"), "digest": format!("sha256:{digest}")
        })];
        if mode != "legacy" {
            assets.push(serde_json::json!({ "name": "manifest.json", "url": format!("{base}/assets/manifest") }));
        }
        if mode == "empty" {
            assets.clear();
        }
        let release = serde_json::json!({
            "tag_name": if mode == "bad-tag" { "not-a-version" } else if mode == "beta-prefix" { "beta-v9.9.9" } else { "v9.9.9" },
            "draft": mode == "draft", "assets": assets
        });
        let manifest = serde_json::json!({
            "version": if mode == "mismatch" { "8.8.8" } else { "9.9.9" },
            "minimumMacOS": "13.0",
            "files": { artifact(): { "sha256": if mode == "bad-checksum" { "0000" } else { &digest } } }
        });
        let mut routes = BTreeMap::from([
            (
                "/releases/tags/v9.9.9".to_owned(),
                release.to_string().into_bytes(),
            ),
            (
                "/releases/latest".to_owned(),
                release.to_string().into_bytes(),
            ),
            (
                "/releases?per_page=20".to_owned(),
                serde_json::json!([release]).to_string().into_bytes(),
            ),
            (
                "/assets/manifest".to_owned(),
                manifest.to_string().into_bytes(),
            ),
            ("/assets/package".to_owned(), PACKAGE.to_vec()),
        ]);
        if mode == "malformed" {
            routes.insert("/releases/latest".into(), b"{".to_vec());
        }
        if mode == "no-releases" {
            routes.insert("/releases?per_page=20".into(), b"[]".to_vec());
        }
        let failing = mode == "error";
        let handle = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut chunk = [0; 2048];
                while !request.windows(4).any(|s| s == b"\r\n\r\n") {
                    let n = socket.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..n]);
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("");
                let body = routes.get(path).map(Vec::as_slice).unwrap_or(b"not found");
                let status = if failing {
                    "503 Unavailable"
                } else if routes.contains_key(path) {
                    "200 OK"
                } else {
                    "404 Not Found"
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                socket.write_all(body).await.unwrap();
            }
        });
        (format!("{base}/releases/latest"), handle)
    }

    #[tokio::test]
    async fn github_manifest_and_legacy_releases_download_through_the_asset_api() {
        for mode in ["normal", "legacy", "beta-prefix"] {
            let (url, server) = server(mode).await;
            let manifest = crate::fetch_latest_on(&url, Channel::Stable).await.unwrap();
            assert_eq!(manifest.version, "9.9.9");
            assert!(manifest.files[&artifact()].sha256.is_some());
            let tmp = tempfile::tempdir().unwrap();
            let dest = tmp.path().join("package");
            crate::download_release_file(&url, &manifest, &artifact(), &dest)
                .await
                .unwrap();
            assert_eq!(std::fs::read(dest).unwrap(), PACKAGE);
            let pinned = url.replace("/latest", "/tags/v9.9.9");
            assert_eq!(
                crate::fetch_latest_on(&pinned, Channel::Beta)
                    .await
                    .unwrap()
                    .version,
                "9.9.9"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn empty_draft_invalid_and_failed_github_feeds_are_not_update_offers() {
        for mode in [
            "empty",
            "draft",
            "malformed",
            "bad-tag",
            "error",
            "mismatch",
        ] {
            let (url, server) = server(mode).await;
            assert!(
                crate::fetch_latest_on(&url, Channel::Stable).await.is_err(),
                "{mode}"
            );
            server.abort();
        }
        let (url, server) = server("no-releases").await;
        assert!(crate::fetch_latest_on(&url, Channel::Beta).await.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn github_checksums_are_enforced_and_beta_also_receives_stable_releases() {
        let (url, server) = server("bad-checksum").await;
        let manifest = crate::fetch_latest_on(&url, Channel::Beta).await.unwrap();
        assert_eq!(manifest.channel, Channel::Beta);
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("package");
        assert!(
            crate::download_release_file(&url, &manifest, &artifact(), &dest)
                .await
                .is_err()
        );
        assert!(!dest.exists());
        assert!(!dest.with_extension("partial").exists());
        server.abort();
    }

    #[test]
    fn recognizes_github_release_api_without_reclassifying_manifest_feeds() {
        assert!(is_feed(crate::DEFAULT_FEED_URL));
        assert!(is_feed("http://localhost/repos/test/releases/latest"));
        assert!(is_feed(
            "https://api.github.com/repos/Mail-0/comet/releases/tags/v1.0.0"
        ));
        assert!(!is_feed("https://releases.example.test"));
    }

    #[test]
    fn github_credentials_never_go_to_an_asset_host() {
        for url in [
            "https://example.test/asset",
            "http://api.github.com/asset",
            "https://api.github.com.evil.test/asset",
        ] {
            let request = request_with_token(
                &reqwest::Client::new(),
                reqwest::Method::GET,
                url,
                Some("private-fixture-token"),
            )
            .build()
            .unwrap();
            assert!(
                !request
                    .headers()
                    .contains_key(reqwest::header::AUTHORIZATION)
            );
            assert_eq!(
                request.headers()[reqwest::header::ACCEPT],
                "application/octet-stream"
            );
        }
        let request = request_with_token(
            &reqwest::Client::new(),
            reqwest::Method::GET,
            crate::DEFAULT_FEED_URL,
            Some("private-fixture-token"),
        )
        .build()
        .unwrap();
        assert_eq!(
            request.headers()[reqwest::header::AUTHORIZATION],
            "Bearer private-fixture-token"
        );
    }

    #[tokio::test]
    async fn credentials_in_an_override_url_are_rejected_before_any_request() {
        let error = crate::fetch_latest_on(
            "https://user:password@api.github.com/repos/Mail-0/comet/releases/latest",
            Channel::Stable,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("without credentials"));
    }
}
