//! Keiki account state and browser sign-in through the engine.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use serde_json::{Value as Json, json};
use zgx_desktop::WindowContext;
use zgx_script::{ScriptHost, Shared, Value};

use crate::engine::{self, rpc::Watch};

const SIGN_IN_WAIT: Duration = Duration::from_secs(10 * 60);
const RESTORE_WAIT: Duration = Duration::from_secs(20);
type Outcome = &'static str;

#[derive(Clone, PartialEq)]
struct User {
    id: String,
    email: String,
    first_name: Option<String>,
    name: Option<String>,
}

/// The engine's session, as its `AuthStatus` says.
#[derive(Clone, PartialEq)]
enum Session {
    /// Not heard yet.
    Unknown,
    SignedOut,
    /// Signed in (`org` once it works in one; none while choosing).
    SignedIn {
        user: User,
        org: Option<String>,
    },
}

struct Auth {
    accounts: RefCell<Vec<Shared>>,
    session: RefCell<Session>,
    /// A sign-in waiting on the browser.
    waiting: Cell<bool>,
    failure: Cell<Option<Outcome>>,
    /// The launch, with no session kept, gave up waiting for the engine:
    /// shown signed out.
    gave_up: Cell<bool>,
    /// Moves on every sign-in, cancel and sign-out: what comes back for an
    /// older one changes nothing.
    generation: Cell<u64>,
    /// Signed out here, as asked: the next sign-in asks for an account
    /// afresh, not the one the browser is still signed into.
    signed_out_here: Cell<bool>,
    /// The engine's `AuthStatus`, on the current connection.
    watch: RefCell<Option<Watch>>,
    started: Cell<bool>,
    /// What waits for the account to work in an organization (`set_up`).
    set_up_waiters: RefCell<Vec<zgx::task::LatestSender<()>>>,
}

/// What the engine answers a call that needs the workspace while the
/// account's setup isn't finished (signed in, no organization chosen: the
/// engine's deferred.rs `SETUP_INCOMPLETE`).
pub const SETUP_INCOMPLETE: &str = "setup_incomplete";

/// Whether an engine error is "the account's setup isn't finished".
pub fn setup_incomplete(error: &str) -> bool {
    error.contains(SETUP_INCOMPLETE)
}

/// Whether the account works in an organization now (`AuthStatus` with an
/// org): the engine answers workspace calls.
pub fn is_set_up() -> bool {
    AUTH.with(|a| matches!(&*a.session.borrow(), Session::SignedIn { org: Some(_), .. }))
}

/// Once the account works in an organization: at once if it does.
pub async fn set_up() {
    if is_set_up() {
        return;
    }
    let (tell, mut heard) = zgx::task::latest::<()>();
    AUTH.with(|a| a.set_up_waiters.borrow_mut().push(tell));
    heard.recv().await;
}

/// `call` (an engine call), again once the account is set up if it answered
/// `setup_incomplete`: what's asked while setup is still being finished
/// (the templates page, a consent window) arrives once it is.
pub async fn after_setup<T, F, Fut>(call: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    retry_after(call, set_up).await
}

/// How many times a call waits for setup before its answer is taken as is.
const SETUP_RETRIES: usize = 3;

async fn retry_after<T, F, Fut, W, WFut>(mut call: F, mut wait: W) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
    W: FnMut() -> WFut,
    WFut: std::future::Future<Output = ()>,
{
    for _ in 0..SETUP_RETRIES {
        match call().await {
            Err(error) if setup_incomplete(&error) => wait().await,
            other => return other,
        }
    }
    call().await
}

thread_local! {
    static AUTH: Rc<Auth> = Rc::new(Auth::new());
}

/// The account for `host`'s scripts: `account` and the `account.*` calls.
/// The first window to ask starts following the engine's session.
pub fn install(cx: &mut WindowContext, host: &ScriptHost) {
    engine::install(cx);
    let auth = AUTH.with(Rc::clone);
    auth.accounts
        .borrow_mut()
        .push(host.shared("account", auth.value()));
    if !auth.started.get() {
        auth.started.set(true);
        auth.clone().follow();
        cx.tasks.spawn(auth.clone().restore_deadline());
    }

    let a = auth.clone();
    host.async_action("account.sign_in", move |kind| {
        let a = a.clone();
        let create = text(&kind) == "create";
        async move {
            crate::plugins::preview::refuse("account.sign_in")?;
            a.sign_in(create).await.map_err(String::from)
        }
    });
    let a = auth.clone();
    host.action("account.cancel", move |_| a.cancel());
    let a = auth.clone();
    host.async_action("account.sign_out", move |_| {
        let a = a.clone();
        async move {
            crate::plugins::preview::refuse("account.sign_out")?;
            a.sign_out().await;
            Ok(Value::Nil)
        }
    });
    let a = auth.clone();
    host.async_action("account.orgs", move |_| {
        let a = a.clone();
        async move {
            crate::plugins::preview::refuse("account.orgs")?;
            a.orgs().await.map_err(String::from)
        }
    });
    let a = auth.clone();
    host.async_action("account.choose_org", move |id| {
        let a = a.clone();
        async move {
            crate::plugins::preview::refuse("account.choose_org")?;
            a.choose_org(text(&id)).await.map_err(String::from)
        }
    });
}

/// Who is signed in, and the organization the session works in (none while
/// choosing one): what a conversation link's workspace is hashed from.
pub(crate) fn signed_in() -> Option<(String, Option<String>)> {
    AUTH.with(|a| match &*a.session.borrow() {
        Session::SignedIn { user, org } => Some((user.id.clone(), org.clone())),
        _ => None,
    })
}

/// The intro is only for a new installation; account state comes from AuthStatus.
pub fn has_local_installation() -> bool {
    engine::data_dir().is_some_and(|dir| dir.join("profiles/local").is_dir())
}

/// Whether a launch still without the engine's answer shows signed out.
fn gives_up_on_session(session_kept: bool) -> bool {
    !session_kept
}

impl Auth {
    fn new() -> Self {
        Self {
            accounts: RefCell::default(),
            session: RefCell::new(Session::Unknown),
            waiting: Cell::new(false),
            failure: Cell::new(None),
            gave_up: Cell::new(false),
            generation: Cell::new(0),
            signed_out_here: Cell::new(false),
            watch: RefCell::default(),
            started: Cell::new(false),
            set_up_waiters: RefCell::default(),
        }
    }

    fn value(&self) -> Value {
        let session = self.session.borrow();
        let state = match &*session {
            Session::SignedIn { .. } => "signed_in",
            _ if self.waiting.get() => "waiting",
            Session::Unknown if !self.gave_up.get() => "restoring",
            _ => "signed_out",
        };
        let user = match &*session {
            Session::SignedIn { user, .. } => Value::record([
                ("id", Value::from(user.id.as_str())),
                ("email", Value::from(user.email.as_str())),
                ("name", user.name.as_deref().map_or(Value::Nil, Value::from)),
                (
                    "first_name",
                    user.first_name.as_deref().map_or(Value::Nil, Value::from),
                ),
            ]),
            _ => Value::Nil,
        };
        Value::record([
            ("state", Value::from(state)),
            ("user", user),
            (
                "failure",
                self.failure.get().map_or(Value::Nil, Value::from),
            ),
            ("env", Value::from("Keiki")),
            // The organization the session works in (Settings' own).
            (
                "organization_id",
                match &*session {
                    Session::SignedIn { org: Some(org), .. } => Value::from(org.as_str()),
                    _ => Value::Nil,
                },
            ),
        ])
    }

    fn publish(&self) {
        let value = self.value();
        for account in self.accounts.borrow().iter() {
            account.set(value.clone());
        }
    }

    fn signed_in(&self) -> bool {
        matches!(&*self.session.borrow(), Session::SignedIn { .. })
    }

    /// Follows the engine's session on every connection. A dropped
    /// connection changes nothing: the next one says where it stands.
    fn follow(self: Rc<Self>) {
        let weak = Rc::downgrade(&self);
        engine::get().on_connect(move |rpc| {
            let Some(auth) = weak.upgrade() else { return };
            let heard = Rc::downgrade(&auth);
            let watch = rpc.watch("AuthStatus", json!({}), move |item| {
                if let Some(auth) = heard.upgrade() {
                    auth.heard(session_from(&item));
                }
            });
            *auth.watch.borrow_mut() = Some(watch);
        });
    }

    fn heard(&self, session: Session) {
        let was_signed_in = self.signed_in();
        let changed = *self.session.borrow() != session;
        let now_signed_in = matches!(session, Session::SignedIn { .. });
        let now_set_up = matches!(session, Session::SignedIn { org: Some(_), .. });
        *self.session.borrow_mut() = session;
        // Setup finished (an organization chosen): what waited for it asks
        // again (`after_setup`).
        if now_set_up {
            for waiter in std::mem::take(&mut *self.set_up_waiters.borrow_mut()) {
                waiter.send(());
            }
        }
        if now_signed_in {
            self.failure.set(None);
        } else if was_signed_in && self.failure.get().is_none() && !self.signed_out_here.get() {
            // Signed out, not by asking here: the session was refused.
            eprintln!("zeron-zgx: account: the session ended");
            self.failure.set(Some("expired"));
        }
        if changed {
            self.publish();
        }
    }

    /// The launch's wait for the engine: with no session kept (or no engine
    /// this app could start to ask), none in time is signed out (the engine's
    /// word shows as soon as it answers). With one kept, only the engine can
    /// say it's gone, whatever keeps it from answering now.
    async fn restore_deadline(self: Rc<Self>) {
        zgx::timer::sleep(RESTORE_WAIT).await;
        let kept = engine::get().can_start();
        if *self.session.borrow() == Session::Unknown && gives_up_on_session(kept) {
            eprintln!("zeron-zgx: account: no answer from the engine yet; shown signed out");
            self.gave_up.set(true);
            self.publish();
        }
    }

    fn next_generation(&self) -> u64 {
        self.generation.set(self.generation.get() + 1);
        self.generation.get()
    }

    /// The engine's sign-in page, opened, then the engine's session awaited.
    async fn sign_in(self: Rc<Self>, create: bool) -> Result<Value, Outcome> {
        if self.signed_in() {
            return Ok(self.value());
        }
        let generation = self.next_generation();
        self.waiting.set(true);
        self.failure.set(None);
        self.publish();
        let result = self.browser_sign_in(create, generation).await;
        if self.generation.get() != generation {
            // Cancelled or signed out meanwhile: that already said so.
            return Err("cancelled");
        }
        self.waiting.set(false);
        match result {
            Ok(()) => {
                self.signed_out_here.set(false);
                self.publish();
                eprintln!("zeron-zgx: account: signed in");
                Ok(self.value())
            }
            Err(outcome) => {
                eprintln!("zeron-zgx: account: sign-in didn't finish ({outcome})");
                self.failure.set(Some(outcome));
                self.publish();
                Err(outcome)
            }
        }
    }

    async fn browser_sign_in(&self, _create: bool, generation: u64) -> Result<(), Outcome> {
        let answer = engine::get()
            .call_soon("SignIn", json!({}))
            .await
            .map_err(|error| {
                eprintln!("zeron-zgx: account: the engine didn't start a sign-in: {error}");
                engine::outcome(&error)
            })?;
        let url = answer["url"].as_str().filter(|url| !url.is_empty()).ok_or("failed")?;
        if !authorization_url_allowed(url) { return Err("failed"); }
        if let Err(error) = open_sign_in(url) {
            eprintln!("zeron-zgx: account: the browser did not open: {error}");
            return Err("failed");
        }
        let until = Instant::now() + SIGN_IN_WAIT;
        loop {
            if self.generation.get() != generation {
                return Err("cancelled");
            }
            if self.signed_in() {
                return Ok(());
            }
            if Instant::now() >= until {
                return Err("timed_out");
            }
            zgx::timer::sleep(Duration::from_millis(200)).await;
        }
    }

    fn cancel(&self) {
        if !self.waiting.get() {
            return;
        }
        self.next_generation();
        self.waiting.set(false);
        self.failure.set(Some("cancelled"));
        self.publish();
    }

    /// Signed out: the engine's session ended (the engine forgets it), as
    /// asked here.
    async fn sign_out(&self) {
        self.next_generation();
        self.waiting.set(false);
        self.signed_out_here.set(true);
        if let Err(error) = engine::get().call("SignOut", json!({})).await {
            eprintln!("zeron-zgx: account: the engine didn't sign out: {error}");
        }
        *self.session.borrow_mut() = Session::SignedOut;
        self.failure.set(None);
        self.publish();
        eprintln!("zeron-zgx: account: signed out");
    }

    /// The organizations reported by the engine's Keiki account:
    /// `{ id, organization_id, name, role }` each (`role` "admin" for the
    /// account's own).
    async fn orgs(&self) -> Result<Value, Outcome> {
        match engine::get().call("ListOrgs", json!({})).await {
            Ok(body) => {
                let orgs: Vec<Value> = body["orgs"]
                    .as_array()
                    .map(|orgs| {
                        orgs.iter()
                            .map(|o| {
                                let str =
                                    |key: &str| o[key].as_str().map_or(Value::Nil, Value::from);
                                Value::record([
                                    ("id", str("id")),
                                    ("organization_id", str("organizationId")),
                                    ("name", str("name")),
                                ])
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(Value::List(orgs.into()))
            }
            _ => Err("failed"),
        }
    }

    /// Switch the Keiki organization without changing the local workspace.
    async fn choose_org(&self, id: String) -> Result<Value, Outcome> {
        match engine::get()
            .call("SelectOrg", json!({ "organizationId": id }))
            .await
        {
            Ok(_) => Ok(Value::record([(
                "organization_id",
                Value::from(id.as_str()),
            )])),
            Err(error) => {
                let lower = error.to_ascii_lowercase();
                if lower.contains("could not switch") || lower.contains("no longer be a member") {
                    Err("refused")
                } else {
                    Err(engine::outcome(&error))
                }
            }
        }
    }
}

/// The engine's `AuthStatus` (`{state: signedOut | needsOrganization |
/// signedIn, user?, orgId?}`) as a session. Choosing an organization counts
/// as signed in: setup's next page is where one is chosen.
fn session_from(item: &Json) -> Session {
    let user = || {
        let u = &item["user"];
        Some(User {
            id: u["id"].as_str()?.into(),
            email: u["email"].as_str()?.into(),
            name: u["name"].as_str().map(Into::into),
            // The engine keeps the whole name; setup greets by the first.
            first_name: u["name"]
                .as_str()
                .and_then(|name| name.split_whitespace().next())
                .map(Into::into),
        })
    };
    match item["state"].as_str() {
        Some("signedIn") | Some("needsOrganization") => match user() {
            Some(user) => Session::SignedIn {
                user,
                org: item["orgId"].as_str().map(Into::into),
            },
            None => Session::SignedOut,
        },
        _ => Session::SignedOut,
    }
}

fn authorization_url_allowed(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            || (url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
    })
}

pub fn open_sign_in(url: &str) -> std::io::Result<()> {
    if no_browser(url) {
        return Ok(());
    }
    if cfg!(target_os = "macos") && std::env::var_os("ZERON_ZGX_PRIVATE_SIGNIN").is_some() {
        return std::process::Command::new("/usr/bin/open")
            .args(["-na", "Google Chrome", "--args", "--incognito", url])
            .spawn()
            .map(|_| ());
    }
    open::that_detached(url)
}

/// `ZERON_ZGX_NO_BROWSER=1` (development: trying a flow without a browser
/// window opening on this Mac): the page isn't opened, only its host said.
/// Set to a file's path, the page goes in that file (owner-only), for a
/// test rig's own browser to open.
pub fn no_browser(url: &str) -> bool {
    let Some(setting) = crate::dev::smoke_switch("ZERON_ZGX_NO_BROWSER") else {
        return false;
    };
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?']).next())
        .unwrap_or("");
    eprintln!("zeron-zgx: (not opened: a page on {host})");
    if setting != "1" {
        #[cfg(unix)]
        {
            use std::{io::Write, os::unix::fs::OpenOptionsExt};
            let written = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&setting)
                .and_then(|mut file| file.write_all(url.as_bytes()));
            if let Err(error) = written {
                eprintln!("zeron-zgx: the page couldn't be written for the rig: {error}");
            }
        }
    }
    true
}

pub fn text(value: &Value) -> String {
    match value {
        Value::Str(s) => s.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_authorization_requires_https_or_a_loopback_test_server() {
        for url in ["https://onkeiki.com/authorize", "http://127.0.0.1:8000/authorize", "http://localhost:8000/authorize", "http://[::1]:8000/authorize"] {
            assert!(authorization_url_allowed(url), "{url}");
        }
        for url in ["", "not a url", "file:///etc/passwd", "javascript:alert(1)", "http://example.test/authorize", "http://localhost.example.test/authorize"] {
            assert!(!authorization_url_allowed(url), "{url}");
        }
    }

    /// A future run to its end on this thread (its waker unparks it).
    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        struct Unpark(std::thread::Thread);
        impl std::task::Wake for Unpark {
            fn wake(self: std::sync::Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = std::task::Waker::from(std::sync::Arc::new(Unpark(std::thread::current())));
        let mut cx = std::task::Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let std::task::Poll::Ready(v) = future.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::park_timeout(std::time::Duration::from_millis(50));
        }
    }

    /// A kept session is signed in until the engine says otherwise, however
    /// long it takes to answer; with none kept, sign-in is shown as before.
    #[test]
    fn a_kept_session_is_never_given_up_on() {
        assert!(!gives_up_on_session(true));
        assert!(gives_up_on_session(false));
    }

    /// While the account's setup isn't finished the engine answers template
    /// (and plugin) calls `setup_incomplete`: the call waits for setup, then
    /// asks again, and gets its answer; any other error is the answer.
    #[test]
    fn a_call_answered_setup_incomplete_asks_again_once_set_up() {
        let asked = Cell::new(0);
        let waited = Cell::new(0);
        let got = block_on(retry_after(
            || {
                asked.set(asked.get() + 1);
                let n = asked.get();
                async move {
                    if n == 1 {
                        Err::<&str, _>("capability: setup_incomplete".to_string())
                    } else {
                        Ok("templates")
                    }
                }
            },
            || {
                waited.set(waited.get() + 1);
                async {}
            },
        ));
        assert_eq!((got, asked.get(), waited.get()), (Ok("templates"), 2, 1));
        // Another error isn't retried.
        asked.set(0);
        let got = block_on(retry_after(
            || {
                asked.set(asked.get() + 1);
                async { Err::<(), _>("offline".to_string()) }
            },
            || async {},
        ));
        assert_eq!((got, asked.get()), (Err("offline".to_string()), 1));
        // Setup never finishing: given up on after a few, the error kept.
        asked.set(0);
        let got = block_on(retry_after(
            || {
                asked.set(asked.get() + 1);
                async { Err::<(), _>(SETUP_INCOMPLETE.to_string()) }
            },
            || async {},
        ));
        assert!(got.is_err_and(|e| setup_incomplete(&e)) && asked.get() == SETUP_RETRIES + 1);
    }

    /// `set_up` resolves once AuthStatus names an organization.
    #[test]
    fn set_up_resolves_when_an_organization_is_chosen() {
        let auth = AUTH.with(Rc::clone);
        auth.heard(session_from(
            &json!({ "state": "needsOrganization", "user": { "id": "u", "email": "a@b.c" } }),
        ));
        assert!(!is_set_up());
        let (tell, mut heard) = zgx::task::latest::<()>();
        auth.set_up_waiters.borrow_mut().push(tell);
        auth.heard(session_from(&json!({ "state": "signedIn", "orgId": "org_1", "user": { "id": "u", "email": "a@b.c" } })));
        assert!(is_set_up());
        assert_eq!(block_on(heard.recv()), Some(()));
        assert!(auth.set_up_waiters.borrow().is_empty());
        block_on(set_up());
    }

    #[test]
    fn the_engines_auth_status_reads_as_a_session() {
        assert!(matches!(
            session_from(&json!({ "state": "signedOut" })),
            Session::SignedOut
        ));
        let user = json!({ "id": "u1", "email": "a@example.com", "name": "Ada Lovelace" });
        match session_from(&json!({ "state": "signedIn", "user": user, "orgId": "org_1" })) {
            Session::SignedIn { user, org } => {
                assert_eq!(user.first_name.as_deref(), Some("Ada"));
                assert_eq!(org.as_deref(), Some("org_1"));
            }
            _ => panic!("signed in"),
        }
        // Choosing an organization is signed in, with none yet.
        assert!(matches!(
            session_from(&json!({ "state": "needsOrganization", "user": user })),
            Session::SignedIn { org: None, .. }
        ));
        // A user the engine didn't send can't be signed in.
        assert!(matches!(
            session_from(&json!({ "state": "signedIn" })),
            Session::SignedOut
        ));
    }
}
