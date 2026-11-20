use std::collections::BTreeMap;

use serde_json::{Value, json};
use tokio::{io::AsyncReadExt, task::JoinHandle};

use super::*;

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
}

struct Request {
    headers: BTreeMap<String, String>,
    body: String,
}

impl Request {
    fn form(&self) -> BTreeMap<String, String> {
        form_urlencoded::parse(self.body.as_bytes())
            .into_owned()
            .collect()
    }
}

struct Server {
    url: String,
    requests: JoinHandle<Vec<Request>>,
}

impl Server {
    async fn start(script: impl FnOnce(&str) -> Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let replies = script(&url);
        let requests = tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in replies {
                let request = tokio::time::timeout(Duration::from_secs(10), async {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    stream.read_line(&mut line).await.unwrap();
                    assert_eq!(line.trim_end(), format!("{} {} HTTP/1.1", reply.method, reply.path));
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
                    let length = headers.get("content-length").map_or(0, |n| n.parse::<usize>().unwrap());
                    let mut body = vec![0; length];
                    stream.read_exact(&mut body).await.unwrap();
                    let body = String::from_utf8(body).unwrap();
                    let response = serde_json::to_string(&reply.body).unwrap();
                    let response = format!(
                        "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        reply.status, response.len(), response,
                    );
                    stream.get_mut().write_all(response.as_bytes()).await.unwrap();
                    Request { headers, body }
                }).await.expect("OAuth client did not complete the expected request");
                requests.push(request);
            }
            requests
        });
        Self { url, requests }
    }

    async fn finish(self) -> Vec<Request> {
        self.requests.await.unwrap()
    }
}

fn discovery(url: &str) -> Vec<Reply> {
    vec![
        Reply::ok(
            "GET",
            "/.well-known/oauth-authorization-server",
            json!({
                "issuer": url,
                "authorization_endpoint": format!("{url}/oauth/authorize"),
                "token_endpoint": format!("{url}/oauth/token"),
                "registration_endpoint": format!("{url}/oauth/register"),
                "revocation_endpoint": format!("{url}/oauth/revoke"),
                "code_challenge_methods_supported": ["S256"],
                "scopes_supported": ["manage", "offline_access"],
            }),
        ),
        Reply::ok(
            "GET",
            "/.well-known/oauth-protected-resource/api/webapp",
            json!({
                "resource": format!("{url}/api/webapp"),
                "authorization_servers": [url],
                "scopes_supported": ["manage"],
                "bearer_methods_supported": ["header"],
            }),
        ),
    ]
}

fn tokens(access: &str, refresh: &str) -> Value {
    json!({
        "access_token": access, "refresh_token": refresh,
        "token_type": "Bearer", "expires_in": 3600, "scope": "manage offline_access",
    })
}

fn session(active: Option<&str>, orgs: Value) -> Value {
    json!({"user": {
        "name": "Test User", "email": "oauth@example.test", "role": "member",
        "orgId": active, "activeOrgId": active, "orgs": orgs,
    }})
}

#[tokio::test]
async fn oauth_loop_discovers_registers_exchanges_refreshes_switches_and_revokes() {
    let (listener, redirect) = bind_loopback_listener().await.unwrap();
    let orgs = json!([
        {"id": "org-1", "name": "First", "role": "member", "logoUrl": null},
        {"id": "org-2", "name": "Second", "role": "admin", "logoUrl": null},
    ]);
    let server = Server::start(|url| {
        let mut replies = discovery(url);
        replies.extend([
            Reply::ok(
                "POST",
                "/oauth/register",
                json!({
                    "client_id": "desktop-client", "redirect_uris": [&redirect],
                    "token_endpoint_auth_method": "none",
                }),
            ),
            Reply::ok("POST", "/oauth/token", tokens("access-1", "refresh-1")),
            Reply::ok(
                "GET",
                "/api/webapp/auth/session",
                session(Some("org-1"), orgs.clone()),
            ),
            Reply::ok("POST", "/oauth/token", tokens("access-2", "refresh-2")),
            Reply::ok(
                "POST",
                "/api/webapp/orgs/switch",
                json!({"ok": true, "activeOrgId": "org-2"}),
            ),
            Reply::ok(
                "GET",
                "/api/webapp/auth/session",
                session(Some("org-2"), orgs),
            ),
            Reply::ok("POST", "/oauth/revoke", json!({})),
        ]);
        replies
    })
    .await;
    let client = Client::new(&server.url);
    client
        .discover_oauth("manage offline_access")
        .await
        .unwrap();
    let client_id = client.register_client(&redirect).await.unwrap();
    let flow = AuthorizationFlow::new(client_id, redirect.clone());
    let authorize = client
        .authorization_url(&flow, "manage offline_access")
        .unwrap();
    let query: BTreeMap<String, String> = authorize.query_pairs().into_owned().collect();
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["resource"], format!("{}/api/webapp", server.url));
    assert_eq!(query["redirect_uri"], redirect);
    let mut callback = Url::parse(&redirect).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("code", "authorization-code")
        .append_pair("state", &query["state"]);
    let callback_task = tokio::spawn(wait_for_loopback_callback(listener, redirect.clone()));
    let response = reqwest::get(callback).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let callback = callback_task.await.unwrap().unwrap();
    let code = flow.authorization_code(&callback).unwrap();
    let token = client.exchange_code(&flow, &code).await.unwrap();
    assert!(!token.should_refresh());
    let account = client.session(token.access_token()).await.unwrap();
    assert_eq!(account.user.email.as_deref(), Some("oauth@example.test"));
    assert_eq!(account.user.orgs.len(), 2);
    assert_eq!(account.user.active_org_id.as_deref(), Some("org-1"));
    let stored = flow.stored_credentials(&token);
    let token = client.refresh_token(&stored).await.unwrap();
    let rotated = token.stored_credentials(stored.client_id);
    assert_eq!(rotated.refresh_token, "refresh-2");
    assert_eq!(token.access_token(), "access-2");
    let switched = client
        .switch_org(token.access_token(), "org-2")
        .await
        .unwrap();
    assert!(switched.ok);
    assert_eq!(switched.active_org_id, "org-2");
    assert_eq!(
        client
            .session(token.access_token())
            .await
            .unwrap()
            .user
            .active_org_id
            .as_deref(),
        Some("org-2")
    );
    client.revoke_token(&rotated.refresh_token).await.unwrap();
    let requests = server.finish().await;
    let registration: Value = serde_json::from_str(&requests[2].body).unwrap();
    assert_eq!(
        registration,
        json!({"client_name": "Keiki Desktop", "redirect_uris": [redirect]})
    );
    let exchange = requests[3].form();
    assert_eq!(exchange["grant_type"], "authorization_code");
    assert_eq!(exchange["code"], "authorization-code");
    assert_eq!(exchange["client_id"], "desktop-client");
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(exchange["code_verifier"].as_bytes())),
        query["code_challenge"]
    );
    assert_eq!(exchange["redirect_uri"], redirect);
    assert_eq!(requests[4].headers["authorization"], "Bearer access-1");
    assert_eq!(
        requests[5].form(),
        BTreeMap::from([
            ("grant_type".into(), "refresh_token".into()),
            ("client_id".into(), "desktop-client".into()),
            ("refresh_token".into(), "refresh-1".into()),
        ])
    );
    assert_eq!(requests[6].headers["authorization"], "Bearer access-2");
    assert_eq!(
        serde_json::from_str::<Value>(&requests[6].body).unwrap(),
        json!({"orgId": "org-2"})
    );
    assert_eq!(requests[7].headers["authorization"], "Bearer access-2");
    assert_eq!(requests[8].form()["token"], "refresh-2");
}

#[tokio::test]
async fn a_session_without_organizations_preserves_the_empty_list() {
    let server = Server::start(|_| {
        vec![Reply::ok(
            "GET",
            "/api/webapp/auth/session",
            session(None, json!([])),
        )]
    })
    .await;
    let account = Client::new(&server.url).session("access").await.unwrap();
    assert!(account.user.orgs.is_empty());
    assert!(account.user.active_org_id.is_none());
    server.finish().await;
}

#[tokio::test]
async fn rejected_refresh_and_rejected_session_are_authentication_failures() {
    let server = Server::start(|_| {
        vec![
            Reply {
                method: "POST",
                path: "/oauth/token",
                status: 400,
                body: json!({"error": "invalid_grant"}),
            },
            Reply {
                method: "GET",
                path: "/api/webapp/auth/session",
                status: 401,
                body: json!({"error": "unauthorized"}),
            },
            Reply {
                method: "GET",
                path: "/api/webapp/auth/session",
                status: 403,
                body: json!({"error": "forbidden"}),
            },
        ]
    })
    .await;
    let client = Client::new(&server.url);
    let stored = StoredCredentials {
        client_id: "desktop-client".into(),
        refresh_token: "expired".into(),
    };
    assert!(
        client
            .refresh_token(&stored)
            .await
            .unwrap_err()
            .is_invalid_refresh_token()
    );
    for _ in 0..2 {
        assert!(
            client
                .session("rejected")
                .await
                .unwrap_err()
                .is_authentication_failure()
        );
    }
    server.finish().await;
}
