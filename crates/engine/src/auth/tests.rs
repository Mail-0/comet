use std::collections::BTreeMap;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use super::*;

#[derive(Default)]
struct MemoryStore(Mutex<Option<SavedCredentials>>);

impl CredentialStore for MemoryStore {
    fn load(&self) -> Result<Option<SavedCredentials>, String> {
        Ok(lock(&self.0).clone())
    }
    fn save(&self, credentials: &SavedCredentials) -> Result<(), String> {
        *lock(&self.0) = Some(credentials.clone());
        Ok(())
    }
    fn clear(&self) -> Result<(), String> {
        *lock(&self.0) = None;
        Ok(())
    }
}

struct Reply {
    method: &'static str,
    path: &'static str,
    status: u16,
    body: Value,
}

impl Reply {
    fn ok(method: &'static str, path: &'static str, body: Value) -> Self {
        Self {
            method,
            path,
            status: 200,
            body,
        }
    }
    fn error(method: &'static str, path: &'static str, status: u16) -> Self {
        Self {
            method,
            path,
            status,
            body: json!({"error": "invalid_grant"}),
        }
    }
}

struct Server {
    url: String,
    done: tokio::task::JoinHandle<Vec<(BTreeMap<String, String>, String)>>,
}

impl Server {
    async fn start(script: impl FnOnce(&str) -> Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let replies = script(&url);
        let done = tokio::spawn(async move {
            let mut requests = Vec::new();
            for mut reply in replies {
                let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                assert_eq!(
                    line.trim_end(),
                    format!("{} {} HTTP/1.1", reply.method, reply.path)
                );
                let mut headers = BTreeMap::new();
                loop {
                    line.clear();
                    stream.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (name, value) = line.trim_end().split_once(':').unwrap();
                    headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
                }
                let length = headers
                    .get("content-length")
                    .map_or(0, |value| value.parse().unwrap());
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                let body = String::from_utf8(body).unwrap();
                if reply.path == "/oauth/register" {
                    let registration: Value = serde_json::from_str(&body).unwrap();
                    reply.body["redirect_uris"] = registration["redirect_uris"].clone();
                }
                requests.push((headers, body));
                let body = reply.body.to_string();
                stream.write_all(format!("HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", reply.status, body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        Self { url, done }
    }
}

fn discovery(url: &str) -> Vec<Reply> {
    vec![
        Reply::ok(
            "GET",
            "/.well-known/oauth-authorization-server",
            json!({
                "issuer": url, "authorization_endpoint": format!("{url}/oauth/authorize"),
                "token_endpoint": format!("{url}/oauth/token"), "registration_endpoint": format!("{url}/oauth/register"),
                "revocation_endpoint": format!("{url}/oauth/revoke"), "code_challenge_methods_supported": ["S256"],
                "scopes_supported": ["manage", "offline_access"],
            }),
        ),
        Reply::ok(
            "GET",
            "/.well-known/oauth-protected-resource/api/webapp",
            json!({
                "resource": format!("{url}/api/webapp"), "authorization_servers": [url],
                "scopes_supported": ["manage"], "bearer_methods_supported": ["header"],
            }),
        ),
        Reply::ok(
            "POST",
            "/oauth/register",
            json!({"client_id": "desktop-client", "token_endpoint_auth_method": "none"}),
        ),
    ]
}

fn tokens(access: &str, refresh: &str) -> Value {
    json!({"access_token": access, "refresh_token": refresh, "expires_in": 3600,
        "scope": "manage offline_access", "token_type": "Bearer"})
}

fn account(org: Option<&str>, orgs: Value) -> Value {
    json!({"user": {"name": "Keiki User", "email": "  USER@Example.Test ",
        "orgId": org, "activeOrgId": org, "role": "member", "orgs": orgs}})
}

fn auth(server: &Server, saved: bool) -> (Auth, Arc<MemoryStore>, CopilotCredentialHolder) {
    let store = Arc::new(MemoryStore::default());
    if saved {
        store
            .save(&SavedCredentials {
                credentials: StoredCredentials {
                    client_id: "desktop-client".into(),
                    refresh_token: "saved-refresh".into(),
                },
                issuer: Some(server.url.clone()),
            })
            .unwrap();
    }
    let auth = Auth::with_store(AuthConfig::new(&server.url, PathBuf::new()), store.clone());
    let holder = CopilotCredentialHolder::default();
    auth.attach_copilot_credentials(holder.clone());
    (auth, store, holder)
}

#[tokio::test]
async fn the_initial_status_restores_secure_credentials_before_publishing_the_gate() {
    use futures::StreamExt;
    use zeron_rpc::{RpcReply, RpcService};

    for mode in ["empty", "saved", "rejected"] {
        let server = Server::start(|_| match mode {
            "saved" => vec![
                Reply::ok("POST", "/oauth/token", tokens("restored-access", "rotated")),
                Reply::ok("GET", "/api/webapp/auth/session", account(Some("org-1"), json!([]))),
            ],
            "rejected" => vec![Reply::error("POST", "/oauth/token", 400)],
            _ => vec![],
        }).await;
        let (auth, store, holder) = auth(&server, mode != "empty");
        let rpc = crate::rpc::AuthRpc::new(auth);
        let RpcReply::Stream(mut stream) = rpc.handle("AuthStatus", json!({})).await.unwrap() else {
            panic!("AuthStatus must remain a watch stream");
        };
        let snapshot = stream.next().await.unwrap();
        assert_eq!(snapshot["state"], if mode == "saved" { "signedIn" } else { "signedOut" });
        assert_eq!(store.load().unwrap().is_some(), mode == "saved");
        assert_eq!(holder.snapshot().is_some(), mode == "saved");
        if mode == "saved" { assert_eq!(snapshot["user"]["id"], "user@example.test"); }
        server.done.await.unwrap();
    }
}

#[tokio::test]
async fn browser_sign_in_restores_account_switches_org_refreshes_and_signs_out() {
    let orgs = json!([{"id": "org-1", "name": "First"}, {"id": "org-2", "name": "Second"}]);
    let server = Server::start(|url| {
        let mut replies = discovery(url);
        replies.extend([
            Reply::ok("POST", "/oauth/token", tokens("access-1", "refresh-1")),
            Reply::ok(
                "GET",
                "/api/webapp/auth/session",
                account(Some("org-1"), orgs.clone()),
            ),
            Reply::ok(
                "GET",
                "/api/webapp/auth/session",
                account(Some("org-1"), orgs.clone()),
            ),
            Reply::ok(
                "POST",
                "/api/webapp/orgs/switch",
                json!({"ok": true, "activeOrgId": "org-2"}),
            ),
            Reply::ok(
                "GET",
                "/api/webapp/auth/session",
                account(Some("org-2"), orgs.clone()),
            ),
            Reply::ok("POST", "/oauth/token", tokens("access-2", "refresh-2")),
            Reply::ok(
                "GET",
                "/api/webapp/auth/session",
                account(Some("org-2"), orgs),
            ),
            Reply::ok("POST", "/oauth/revoke", json!({})),
        ]);
        replies
    })
    .await;
    let (auth, store, holder) = auth(&server, false);
    let mut states = auth.watch_state();
    use zeron_rpc::{RpcReply, RpcService};
    let rpc = crate::rpc::AuthRpc::new(auth.clone());
    let RpcReply::Value(answer) = rpc
        .handle(zeron_rpc::methods::SIGN_IN, json!({}))
        .await
        .unwrap()
    else {
        panic!("SignIn must return the authorization URL");
    };
    let url = reqwest::Url::parse(answer["url"].as_str().unwrap()).unwrap();
    let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    let mut callback = reqwest::Url::parse(&query["redirect_uri"]).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("state", &query["state"])
        .append_pair("code", "code-1");
    assert!(reqwest::get(callback).await.unwrap().status().is_success());
    tokio::time::timeout(
        Duration::from_secs(5),
        states.wait_for(|state| state.is_signed_in()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(auth.user_id().as_deref(), Some("user@example.test"));
    assert_eq!(auth.state().org_id(), Some("org-1"));
    assert_eq!(holder.snapshot().unwrap().access_token, "access-1");
    assert_eq!(holder.snapshot().unwrap().base_url, server.url);
    let metadata = serde_json::to_value(store.load().unwrap().unwrap()).unwrap();
    assert_eq!(
        metadata,
        json!({"client_id": "desktop-client", "refresh_token": "refresh-1", "issuer": server.url})
    );
    assert_eq!(auth.list_orgs().await.unwrap()[1].organization_id, "org-2");
    auth.select_org("org-2").await.unwrap();
    assert_eq!(auth.state().org_id(), Some("org-2"));
    let attached_later = CopilotCredentialHolder::default();
    auth.attach_copilot_credentials(attached_later.clone());
    assert_eq!(attached_later.snapshot().unwrap().access_token, "access-1");
    lock(&auth.inner.session).access.as_mut().unwrap().lifetime = Duration::ZERO;
    assert_eq!(auth.access_token().await.unwrap(), "access-2");
    assert_eq!(holder.snapshot().unwrap().access_token, "access-2");
    assert_eq!(attached_later.snapshot().unwrap().access_token, "access-2");
    assert!(auth.access_token_with_expiry().await.unwrap().1.is_some());
    auth.sign_out().await.unwrap();
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(holder.snapshot().is_none());
    assert!(attached_later.snapshot().is_none());
    assert!(store.load().unwrap().is_none());
    assert!(matches!(
        auth.access_token().await,
        Err(TokenError::SignedOut)
    ));
    let requests = server.done.await.unwrap();
    assert!(requests.last().unwrap().1.contains("refresh-2"));
    assert_eq!(requests[4].0["authorization"], "Bearer access-1");
}

#[tokio::test]
async fn an_account_without_organizations_waits_for_selection() {
    let server = Server::start(|_| {
        vec![
            Reply::ok("POST", "/oauth/token", tokens("access", "rotated")),
            Reply::ok("GET", "/api/webapp/auth/session", account(None, json!([]))),
            Reply::ok("GET", "/api/webapp/auth/session", account(None, json!([]))),
        ]
    })
    .await;
    let (auth, _, holder) = auth(&server, true);
    auth.access_token().await.unwrap();
    assert!(matches!(auth.state(), AuthState::NeedsOrganization { .. }));
    assert!(auth.list_orgs().await.unwrap().is_empty());
    assert!(holder.snapshot().is_some());
    server.done.await.unwrap();
}

#[tokio::test]
async fn missing_or_blank_email_clears_credentials_and_copilot() {
    for email in [Value::Null, json!(" \t ")] {
        let server = Server::start(|_| {
            let mut body = account(Some("org-1"), json!([]));
            body["user"]["email"] = email;
            vec![
                Reply::ok("POST", "/oauth/token", tokens("access", "rotated")),
                Reply::ok("GET", "/api/webapp/auth/session", body),
            ]
        })
        .await;
        let (auth, store, holder) = auth(&server, true);
        assert!(matches!(
            auth.access_token().await,
            Err(TokenError::SignedOut)
        ));
        assert_eq!(auth.state(), AuthState::SignedOut);
        assert!(store.load().unwrap().is_none());
        assert!(holder.snapshot().is_none());
        server.done.await.unwrap();
    }
}

#[tokio::test]
async fn invalid_refresh_and_rejected_refreshed_sessions_require_sign_in() {
    for status in [400, 401, 403] {
        let server = Server::start(|_| vec![Reply::error("POST", "/oauth/token", status)]).await;
        let (auth, store, holder) = auth(&server, true);
        assert!(matches!(
            auth.access_token().await,
            Err(TokenError::SignedOut)
        ));
        assert_eq!(auth.state(), AuthState::SignedOut);
        assert!(store.load().unwrap().is_none());
        assert!(holder.snapshot().is_none());
        server.done.await.unwrap();
    }
    for status in [401, 403] {
        let server = Server::start(|_| {
            vec![
                Reply::ok("POST", "/oauth/token", tokens("rejected-access", "rotated")),
                Reply::error("GET", "/api/webapp/auth/session", status),
            ]
        })
        .await;
        let (auth, store, holder) = auth(&server, true);
        assert!(matches!(
            auth.access_token().await,
            Err(TokenError::SignedOut)
        ));
        assert_eq!(auth.state(), AuthState::SignedOut);
        assert!(store.load().unwrap().is_none());
        assert!(holder.snapshot().is_none());
        server.done.await.unwrap();
    }
}

#[tokio::test]
async fn a_temporary_session_failure_keeps_the_rotated_refresh_token() {
    let server = Server::start(|_| {
        vec![
            Reply::ok("POST", "/oauth/token", tokens("access-1", "refresh-1")),
            Reply::error("GET", "/api/webapp/auth/session", 503),
            Reply::ok("POST", "/oauth/token", tokens("access-2", "refresh-2")),
            Reply::ok(
                "GET",
                "/api/webapp/auth/session",
                account(Some("org-1"), json!([])),
            ),
        ]
    })
    .await;
    let (auth, store, holder) = auth(&server, true);
    assert!(matches!(
        auth.access_token().await,
        Err(TokenError::TemporarilyUnavailable(_))
    ));
    assert_eq!(
        store.load().unwrap().unwrap().credentials.refresh_token,
        "refresh-1"
    );
    assert!(holder.snapshot().is_none());
    assert_eq!(auth.access_token().await.unwrap(), "access-2");
    let requests = server.done.await.unwrap();
    assert!(requests[2].1.contains("refresh-1"));
    assert_eq!(holder.snapshot().unwrap().access_token, "access-2");
}

#[tokio::test]
async fn sign_out_cancels_pending_sign_in() {
    let server = Server::start(discovery).await;
    let (auth, store, holder) = auth(&server, false);
    let url = reqwest::Url::parse(&auth.start_sign_in().await.unwrap()).unwrap();
    let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    let mut callback = reqwest::Url::parse(&query["redirect_uri"]).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("state", &query["state"])
        .append_pair("code", "old-code");
    auth.sign_out().await.unwrap();
    assert!(auth.complete_sign_in(callback.as_str()).await.is_err());
    assert!(store.load().unwrap().is_none());
    assert!(holder.snapshot().is_none());
    server.done.await.unwrap();
}

#[test]
fn historical_comet_credentials_deserialize_without_profile_or_access_token() {
    let saved: SavedCredentials =
        serde_json::from_value(json!({"client_id": "desktop", "refresh_token": "refresh"}))
            .unwrap();
    assert_eq!(saved.credentials.client_id, "desktop");
    assert_eq!(saved.credentials.refresh_token, "refresh");
    assert!(saved.issuer.is_none());
    assert_eq!(
        (KEYRING_SERVICE, KEYRING_ACCOUNT),
        ("keiki://oauth", "OAuth")
    );
}

#[tokio::test]
async fn credentials_for_another_issuer_are_not_refreshed() {
    let store = Arc::new(MemoryStore::default());
    store
        .save(&SavedCredentials {
            credentials: StoredCredentials {
                client_id: "desktop".into(),
                refresh_token: "production-refresh".into(),
            },
            issuer: None,
        })
        .unwrap();
    let auth = Auth::with_store(AuthConfig::new("http://127.0.0.1:1", PathBuf::new()), store);
    assert!(matches!(
        auth.access_token().await,
        Err(TokenError::SignedOut)
    ));
}

#[tokio::test]
async fn concurrent_token_consumers_share_one_refresh() {
    let server = Server::start(|_| {
        vec![
            Reply::ok("POST", "/oauth/token", tokens("access", "rotated")),
            Reply::ok(
                "GET",
                "/api/webapp/auth/session",
                account(Some("org-1"), json!([])),
            ),
        ]
    })
    .await;
    let (auth, _, _) = auth(&server, true);
    let (first, second) = tokio::join!(auth.access_token(), auth.access_token());
    assert_eq!(first.unwrap(), "access");
    assert_eq!(second.unwrap(), "access");
    assert_eq!(server.done.await.unwrap().len(), 2);
}

#[tokio::test]
async fn refresh_network_errors_keep_the_saved_credentials() {
    let server = Server::start(|_| vec![Reply::error("POST", "/oauth/token", 503)]).await;
    let (auth, store, holder) = auth(&server, true);
    assert!(matches!(
        auth.access_token().await,
        Err(TokenError::TemporarilyUnavailable(_))
    ));
    assert_eq!(
        store.load().unwrap().unwrap().credentials.refresh_token,
        "saved-refresh"
    );
    assert!(holder.snapshot().is_none());
    server.done.await.unwrap();
}
