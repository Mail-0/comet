//! RegistryDoc unit tests for local row merge and typed APIs.

use super::*;
use zeron_proto::{HarnessId, SandboxLevel, SessionStatus};

fn ts(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or(DateTime::UNIX_EPOCH)
}

fn hlc(ms: i64) -> String {
    encode_hlc(ms, 0, "dev-a")
}

fn hlc_by(ms: i64, device: &str) -> String {
    encode_hlc(ms, 0, device)
}

fn upsert(set: &[(&str, Value)], at: i64) -> RowOp {
    RowOp {
        kind: "chats".into(),
        id: "chat-1".into(),
        op: OpKind::Upsert,
        set: Some(
            set.iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        ),
        hlc: hlc(at),
        clocks: None,
    }
}

fn update(set: &[(&str, Value)], hlc: String) -> RowOp {
    RowOp {
        kind: "chats".into(),
        id: "chat-1".into(),
        op: OpKind::Update,
        set: Some(
            set.iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        ),
        hlc,
        clocks: None,
    }
}

fn delete(at: i64) -> RowOp {
    RowOp {
        kind: "chats".into(),
        id: "chat-1".into(),
        op: OpKind::Delete,
        set: None,
        hlc: hlc(at),
        clocks: None,
    }
}

fn applied(row: Option<&RegistryRow>, op: &RowOp) -> RegistryRow {
    let (next, changed) = apply_op(row, op);
    assert!(changed, "expected op to change the row");
    next.expect("row after change")
}

// ── merge semantics (mirror of registry-core.test.ts) ───────────────────────

#[test]
fn hlc_orders_lexicographically() {
    assert!(hlc(2) > hlc(1));
    assert!(encode_hlc(1, 2, "a") > encode_hlc(1, 1, "a"));
    assert!(encode_hlc(1, 1, "b") > encode_hlc(1, 1, "a"));
    assert!(hlc(10_000) > hlc(999));
}

#[test]
fn hlc_clock_is_monotonic_across_regressions() {
    let mut clock = HlcClock::default();
    let a = clock.next(1_000, "d");
    let b = clock.next(500, "d"); // wall clock went backwards
    let c = clock.next(1_000, "d"); // and stalled
    assert!(b > a);
    assert!(c > b);
}

#[test]
fn upsert_creates_update_never_does() {
    let row = applied(None, &upsert(&[("title", json!("hello"))], 1000));
    assert_eq!(row.fields["title"], json!("hello"));
    let (missing, changed) = apply_op(None, &update(&[("title", json!("x"))], hlc(2000)));
    assert!(!changed);
    assert!(missing.is_none());
}

#[test]
fn field_lww_newer_wins_older_and_ties_lose() {
    let row = applied(
        None,
        &upsert(
            &[("title", json!("hello")), ("archived", json!(false))],
            1000,
        ),
    );
    let (_, changed) = apply_op(Some(&row), &update(&[("title", json!("stale"))], hlc(500)));
    assert!(!changed);
    // Exact replay: strict-> compare makes re-pushes idempotent.
    let (_, changed) = apply_op(
        Some(&row),
        &upsert(
            &[("title", json!("hello")), ("archived", json!(false))],
            1000,
        ),
    );
    assert!(!changed);
    let renamed = applied(
        Some(&row),
        &update(&[("title", json!("renamed"))], hlc_by(2000, "dev-b")),
    );
    assert_eq!(renamed.fields["title"], json!("renamed"));
    assert_eq!(renamed.clocks["archived"], hlc(1000));
}

#[test]
fn same_ms_conflicts_settle_by_device_deterministically() {
    let base = applied(None, &upsert(&[("title", json!("hello"))], 1000));
    let from_a = update(&[("title", json!("A"))], hlc_by(5000, "dev-a"));
    let from_b = update(&[("title", json!("B"))], hlc_by(5000, "dev-b"));
    let ab = apply_op(apply_op(Some(&base), &from_a).0.as_ref(), &from_b)
        .0
        .unwrap();
    let ba = apply_op(apply_op(Some(&base), &from_b).0.as_ref(), &from_a)
        .0
        .unwrap();
    assert_eq!(ab.fields["title"], json!("B"));
    assert_eq!(ba.fields["title"], json!("B"));
}

#[test]
fn null_deletes_fields_with_a_clock() {
    let row = applied(
        None,
        &upsert(&[("title", json!("x")), ("name", json!("y"))], 1000),
    );
    let row = applied(Some(&row), &update(&[("name", Value::Null)], hlc(2000)));
    assert!(!row.fields.contains_key("name"));
    assert_eq!(row.clocks["name"], hlc(2000));
    let (_, changed) = apply_op(Some(&row), &update(&[("name", json!("zombie"))], hlc(1500)));
    assert!(!changed);
}

#[test]
fn delete_only_wins_when_causally_newer() {
    let row = applied(None, &upsert(&[("title", json!("hello"))], 1000));
    let (_, changed) = apply_op(Some(&row), &delete(500));
    assert!(!changed);
    let gone = applied(Some(&row), &delete(2000));
    assert!(gone.deleted);
    assert!(gone.fields.is_empty());
    // Updates never touch tombstones.
    let (_, changed) = apply_op(
        Some(&gone),
        &update(&[("title", json!("ghost"))], hlc(3000)),
    );
    assert!(!changed);
    // Older upsert can't revive; newer revives from ONLY its own fields.
    let (_, changed) = apply_op(Some(&gone), &upsert(&[("title", json!("old"))], 1500));
    assert!(!changed);
    let revived = applied(Some(&gone), &upsert(&[("title", json!("back"))], 4000));
    assert!(!revived.deleted);
    assert_eq!(revived.fields.len(), 1);
    assert_eq!(revived.fields["title"], json!("back"));
}

#[test]
fn delete_on_missing_plants_guard_tombstone() {
    let gone = applied(None, &delete(1000));
    assert!(gone.deleted);
    let (_, changed) = apply_op(Some(&gone), &upsert(&[("title", json!("late"))], 500));
    assert!(!changed);
}
// ── doc lifecycle ───────────────────────────────────────────────────────────

fn device(id: &str, name: &str) -> Device {
    Device {
        id: id.into(),
        name: name.into(),
        platform: "linux".into(),
        last_seen_at: Some(ts(1_000)),
        created_at: Some(ts(500)),
        version: Some("0.1.0".into()),
    }
}

fn chat(id: &str, device_id: &str) -> Chat {
    Chat {
        id: id.into(),
        device_id: device_id.into(),
        title: Some("First chat".into()),
        archived: false,
        cwd: Some("/tmp/repo".into()),
        branch: Some("main".into()),
        checkout_id: None,
        source_context: None,
        config: Some(ChatConfig {
            harness: HarnessId::Mock,
            model: Some("mock-1".into()),
            reasoning: None,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
        }),
        last_message_preview: None,
        last_message_at: None,
        created_at: ts(2_000),
        harness_session_id: None,
        harness_session_cwd: None,
        space_id: None,
        last_seen_at: None,
    }
}

fn space(id: &str, device_id: &str, path: &str) -> Space {
    Space {
        id: id.into(),
        device_id: device_id.into(),
        path: path.into(),
        name: None,
        git_detected: false,
        git_checked_at: None,
        checkout_id: None,
        created_at: ts(1_500),
    }
}

fn session(chat_id: &str, device_id: &str, status: SessionStatus) -> Session {
    Session {
        chat_id: chat_id.into(),
        device_id: device_id.into(),
        status,
        started_at: Some(ts(3_000)),
        updated_at: ts(3_500),
    }
}
#[test]
fn rows_round_trip_and_upsert_refreshes() {
    let mut doc = RegistryDoc::new("dev-a");
    doc.upsert_device(&device("dev-a", "laptop")).unwrap();
    doc.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    doc.upsert_session(&session("chat-1", "dev-a", SessionStatus::Working))
        .unwrap();

    let state = doc.read_all().unwrap();
    assert_eq!(state.devices, vec![device("dev-a", "laptop")]);
    assert_eq!(state.chats, vec![chat("chat-1", "dev-a")]);
    assert_eq!(
        state.sessions,
        vec![session("chat-1", "dev-a", SessionStatus::Working)]
    );

    let mut updated = chat("chat-1", "dev-a");
    updated.title = None;
    updated.last_message_preview = Some("hello".into());
    updated.last_message_at = Some(ts(9_000));
    doc.upsert_chat(&updated).unwrap();
    let chats = doc.read_chats().unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].title, None);
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("hello"));
}
#[test]
fn future_harness_chat_rows_stay_visible_without_their_config() {
    // Field incident: a pre-v0.2.10 client received rows whose
    // config.harness said "opencode" — a variant it didn't have — and
    // dropped the WHOLE row ("skipping malformed registry row"), so new
    // sessions silently never appeared in that device's sidebar. Unknown
    // config values must cost the config, not the row.
    let mut ws = RegistryDoc::new("dev-a");
    ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    let fields: std::collections::BTreeMap<String, serde_json::Value> = [
        ("id".to_owned(), json!("chat-2")),
        ("deviceId".to_owned(), json!("dev-b")),
        ("createdAt".to_owned(), json!(1_000)),
        (
            "config".to_owned(),
            json!({
                "harness": "harness-from-the-future",
                "model": "novel/model",
                "sandbox": "workspace-write",
            }),
        ),
    ]
    .into_iter()
    .collect();
    ws.put_authoritative(RegistryRow {
        kind: "chats".into(),
        id: "chat-2".into(),
        deleted: false,
        del_hlc: None,
        fields,
        clocks: Default::default(),
    });

    let chats = ws.read_chats().unwrap();
    assert_eq!(chats.len(), 2, "the future-harness row must not vanish");
    let newcomer = chats.iter().find(|c| c.id == "chat-2").expect("visible");
    assert_eq!(
        newcomer.config.as_ref().map(|config| config.harness),
        Some(HarnessId::Unknown("unknown")),
        "unknown harness config remains readable"
    );
    // Both rows keep their configs untouched.
    assert!(chats.iter().any(|c| c.id == "chat-1" && c.config.is_some()));
}

#[test]
fn field_mutators_round_trip() {
    let mut ws = RegistryDoc::new("dev-a");
    ws.upsert_device(&device("dev-a", "laptop")).unwrap();
    ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();

    assert!(ws.rename_chat("chat-1", "Renamed").unwrap());
    assert!(ws.set_chat_archived("chat-1", true).unwrap());
    assert!(
        ws.set_chat_last_message("chat-1", "preview text", ts(5_000))
            .unwrap()
    );
    assert!(ws.rename_device("dev-a", "workstation").unwrap());
    assert!(ws.set_device_last_seen("dev-a", ts(6_000)).unwrap());
    assert!(!ws.rename_chat("nope", "x").unwrap());
    assert!(!ws.set_chat_archived("nope", true).unwrap());
    assert!(!ws.rename_device("nope", "x").unwrap());

    let chat = ws.chat("chat-1").unwrap().unwrap();
    assert_eq!(chat.title.as_deref(), Some("Renamed"));
    assert!(chat.archived);
    assert_eq!(chat.last_message_preview.as_deref(), Some("preview text"));
    assert_eq!(chat.last_message_at, Some(ts(5_000)));
    let dev = &ws.read_devices().unwrap()[0];
    assert_eq!(dev.name, "workstation");
    assert_eq!(dev.last_seen_at, Some(ts(6_000)));
}

#[test]
fn delete_chat_tombstones_row_and_session() {
    let mut ws = RegistryDoc::new("dev-a");
    ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    ws.upsert_session(&session("chat-1", "dev-a", SessionStatus::Idle))
        .unwrap();
    assert!(ws.delete_chat("chat-1").unwrap());
    assert!(ws.read_chats().unwrap().is_empty());
    assert!(ws.read_sessions().unwrap().is_empty());
    assert!(!ws.delete_chat("chat-1").unwrap());
}

#[test]
fn spaces_round_trip_and_mutate() {
    let mut ws = RegistryDoc::new("dev-a");
    ws.upsert_space(&space("sp-1", "dev-a", "/home/u/project"))
        .unwrap();
    let row = ws.space("sp-1").unwrap().expect("row exists");
    assert_eq!(row.display_name(), "project");
    assert!(!row.git_detected);

    assert!(ws.rename_space("sp-1", Some("My Project")).unwrap());
    assert_eq!(
        ws.space("sp-1").unwrap().unwrap().display_name(),
        "My Project"
    );
    assert!(ws.rename_space("sp-1", None).unwrap());
    assert_eq!(ws.space("sp-1").unwrap().unwrap().display_name(), "project");

    assert!(
        ws.set_space_git("sp-1", true, Some("checkout-abc"), ts(4_000))
            .unwrap()
    );
    let row = ws.space("sp-1").unwrap().unwrap();
    assert!(row.git_detected);
    assert_eq!(row.checkout_id.as_deref(), Some("checkout-abc"));
    assert_eq!(row.git_checked_at, Some(ts(4_000)));

    assert!(!ws.rename_space("nope", Some("x")).unwrap());
    assert!(!ws.set_space_git("nope", true, None, ts(1)).unwrap());
}

#[test]
fn chat_seen_is_monotonic() {
    let mut ws = RegistryDoc::new("dev-a");
    ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    assert!(ws.set_chat_seen("chat-1", ts(5_000)).unwrap());
    assert_eq!(
        ws.chat("chat-1").unwrap().unwrap().last_seen_at,
        Some(ts(5_000))
    );
    // Older stamps are ignored without a write.
    let before = ws.generation();
    assert!(ws.set_chat_seen("chat-1", ts(4_000)).unwrap());
    assert_eq!(ws.generation(), before);
    assert_eq!(
        ws.chat("chat-1").unwrap().unwrap().last_seen_at,
        Some(ts(5_000))
    );
    assert!(!ws.set_chat_seen("nope", ts(1)).unwrap());
}
#[test]
fn delete_space_cascades_locally() {
    let mut a = RegistryDoc::new("dev-a");
    a.upsert_space(&space("sp-1", "dev-a", "/tmp/one")).unwrap();
    a.upsert_space(&space("sp-2", "dev-a", "/tmp/two")).unwrap();
    let mut in_space = chat("chat-1", "dev-a");
    in_space.space_id = Some("sp-1".into());
    let mut other = chat("chat-2", "dev-a");
    other.space_id = Some("sp-2".into());
    a.upsert_chat(&in_space).unwrap();
    a.upsert_chat(&other).unwrap();
    a.upsert_session(&session("chat-1", "dev-a", SessionStatus::Working))
        .unwrap();
    let deleted = a.delete_space("sp-1").unwrap();
    assert!(deleted.existed);
    assert_eq!(deleted.chat_ids, vec!["chat-1".to_string()]);
    // Overlay hides the cascade locally before the server even sees it.
    assert_eq!(a.read_spaces().unwrap().len(), 1);
    let state = a.read_all().unwrap();
    assert_eq!(
        state
            .spaces
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        vec!["sp-2"]
    );
    assert_eq!(
        state
            .chats
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        vec!["chat-2"]
    );
    assert!(state.sessions.is_empty());
    let again = a.delete_space("sp-1").unwrap();
    assert!(!again.existed);
    assert!(again.chat_ids.is_empty());
}

#[test]
fn persistence_round_trips_rows() {
    let mut doc = RegistryDoc::new("dev-a");
    doc.upsert_chat(&chat("chat-1", "dev-a")).unwrap();

    let bytes = doc.to_bytes().unwrap();
    let restored = RegistryDoc::from_bytes(&bytes, "dev-a").unwrap();
    assert_eq!(restored.read_all().unwrap(), doc.read_all().unwrap());
}
