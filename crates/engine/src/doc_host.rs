//! DocHost — per-chat `SessionDoc` handles and the HOST-ONLY durable command executor.
//!
//! Pragmatic port of comet's `session-docs.ts` + the `main.ts` executor (spec:
//! feature-inventory §3.3, ARCHITECTURE §2 "command plane"):
//! - the doc is the local outbox: commands and user entries commit locally;
//! - on every doc change the handle re-emits the joined transcript to watchers,
//!   drains pending commands, and schedules a snapshot save;
//! - command drain: evaluate via `evaluate_command` (with the DocsStore processed
//!   ledger), mark processed BEFORE execute, execute through the sessions engine, then
//!   write the outcome status back into the doc as the sole outcome writer.
//!
//! Chat ownership is gated on the workspace doc (`chats[chat_id].deviceId`), with
//! claim-on-first-command for unknown chats.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use tokio::sync::watch;

use zeron_doc::DocsStore;
use zeron_doc::{
    COMMAND_DEFAULT_TTL_MS, CommandBasedOn, CommandDisposition, DocError, EvaluationContext,
    MessagePart, MessageRole, MessageStatus, SessionCommandEntry, SessionCommandPayload,
    SessionCommandStatus, SessionDoc, SessionMessageEntry, evaluate_command,
    join_continuation_entries,
};
use zeron_proto::{HarnessId, UserInputAnswer, UserInputQuestion};

use crate::repos::Repos;
use crate::sessions::{SessionsEngine, SteerOutcome};
use crate::workspace_host::WorkspaceHost;
use crate::{EngineError, new_id, now_ms};

/// Debounce window for local snapshot saves after a doc change.
const SNAPSHOT_DEBOUNCE_MS: u64 = 1_000;

#[derive(Debug, Clone)]
pub struct DocHostConfig {
    pub device_id: String,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
}

struct DocHostInner {
    store: Arc<DocsStore>,
    config: DocHostConfig,
    sessions: OnceLock<SessionsEngine>,
    workspace: OnceLock<WorkspaceHost>,
    repos: OnceLock<Repos>,
    handles: Mutex<HashMap<String, Arc<ChatDocHandle>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct DocHost {
    inner: Arc<DocHostInner>,
}

/// One open chat doc: the `SessionDoc` and its change plumbing.
pub struct ChatDocHandle {
    chat_id: String,
    device_id: String,
    doc: Arc<SessionDoc>,
    messages_tx: watch::Sender<Vec<SessionMessageEntry>>,
    /// Doc subscription (drop = unsubscribe) — bumps the change watch on every commit.
    _sub: loro::Subscription,
}

impl ChatDocHandle {
    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    pub fn doc(&self) -> &SessionDoc {
        &self.doc
    }

    pub fn doc_arc(&self) -> Arc<SessionDoc> {
        self.doc.clone()
    }

    /// Joined transcript watch — re-sent on every doc change (WatchDocMessages).
    pub fn watch_messages(&self) -> watch::Receiver<Vec<SessionMessageEntry>> {
        self.messages_tx.subscribe()
    }

    /// Write a complete user message entry, idempotent by id (the client-minted message
    /// id — a re-executed command or optimistic echo never duplicates the entry).
    pub fn write_user_message(
        &self,
        message_id: &str,
        text: &str,
        created_at: i64,
    ) -> Result<(), DocError> {
        if self.doc.read_entries()?.iter().any(|e| e.id == message_id) {
            return Ok(());
        }
        self.doc.push_message(&SessionMessageEntry {
            id: message_id.to_string(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.to_string(),
            }],
            created_at,
            device_id: self.device_id.clone(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
    }

    /// Recovery sweep: stamp this device's abandoned `streaming` assistant entries
    /// `aborted`, appending `note` as a visible error part so the transcript
    /// says why the turn ended.
    pub fn mark_abandoned_streams(&self, note: &str) -> Result<Vec<(String, i64)>, DocError> {
        let mut stamped = Vec::new();
        for entry in self.doc.read_entries()? {
            if entry.role == MessageRole::Assistant
                && entry.status == Some(MessageStatus::Streaming)
                && entry.device_id == self.device_id
                && self
                    .doc
                    .set_message_status(&entry.id, MessageStatus::Aborted)?
            {
                let part_id = format!("{}-recovery", entry.id);
                if let Err(err) = self.doc.append_error_part(&entry.id, &part_id, note) {
                    tracing::warn!(chat = %self.chat_id, error = %err, "recovery note append failed");
                }
                stamped.push((entry.id.clone(), entry.created_at));
            }
        }
        if !stamped.is_empty() {
            self.publish_messages();
        }
        Ok(stamped)
    }

    fn publish_messages(&self) {
        match self.doc.read_entries() {
            Ok(entries) => {
                let joined = join_continuation_entries(entries);
                let _ = self.messages_tx.send(joined);
            }
            Err(err) => {
                tracing::warn!(chat = %self.chat_id, error = %err, "transcript read failed");
            }
        }
    }
}

impl DocHost {
    pub fn new(store: Arc<DocsStore>, config: DocHostConfig) -> Self {
        Self {
            inner: Arc::new(DocHostInner {
                store,
                config,
                sessions: OnceLock::new(),
                workspace: OnceLock::new(),
                repos: OnceLock::new(),
                handles: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Wire the sessions engine (engine assembly; see `SessionsEngine::set_doc_host`).
    pub fn set_sessions(&self, sessions: SessionsEngine) {
        let _ = self.inner.sessions.set(sessions);
        // Commands may already be pending in warm-opened docs.
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            let host = self.clone();
            tokio::spawn(async move { host.drain_commands(&handle).await });
        }
    }

    /// Wire the workspace host (engine assembly) — the source of chat-ownership rows.
    pub fn set_workspace(&self, workspace: WorkspaceHost) {
        let _ = self.inner.workspace.set(workspace);
    }

    pub fn set_repos(&self, repos: Repos) {
        let _ = self.inner.repos.set(repos);
    }

    pub async fn shutdown_workers(&self) {
        self.flush_all();
        lock(&self.inner.handles).clear();
    }

    pub fn request_from_chat_row(
        &self,
        chat_id: &str,
        prompt: &str,
    ) -> Option<zeron_proto::RunRequest> {
        let chat = self.workspace()?.chat(chat_id).ok().flatten()?;
        Some(zeron_proto::RunRequest {
            prompt: prompt.to_string(),
            harness: chat.config.as_ref().map(|c| c.harness),
            model: chat.config.as_ref().and_then(|c| c.model.clone()),
            reasoning: chat.config.as_ref().and_then(|c| c.reasoning),
            model_options: chat
                .config
                .as_ref()
                .map(|c| c.model_options.clone())
                .unwrap_or_default(),
            cwd: chat
                .cwd
                .unwrap_or_else(|| crate::repos::home_dir().to_string_lossy().to_string()),
            sandbox: chat
                .config
                .as_ref()
                .map(|c| c.sandbox)
                .unwrap_or(zeron_proto::SandboxLevel::WorkspaceWrite),
            auto_approve: false,
            resume: None,
            attachments: Vec::new(),
            worktree: None,
        })
    }

    pub fn harness_for_request(
        &self,
        chat_id: &str,
        request: &zeron_proto::RunRequest,
    ) -> HarnessId {
        request.harness.unwrap_or_else(|| self.harness_for(chat_id))
    }

    pub async fn dispatch_with_source_context(
        &self,
        sessions: &SessionsEngine,
        chat_id: &str,
        harness: HarnessId,
        request: zeron_proto::RunRequest,
        message_id: Option<String>,
    ) -> Result<String, EngineError> {
        sessions
            .dispatch(chat_id, harness, request, message_id)
            .await
    }

    async fn materialize_worktree(
        &self,
        chat_id: &str,
        spec: &zeron_proto::WorktreeSpec,
    ) -> Result<(String, Option<zeron_proto::Worktree>), EngineError> {
        if let Some(workspace) = self.workspace()
            && let Some(chat) = workspace.chat(chat_id)?
            && let Some(cwd) = chat.cwd
            && cwd != spec.repo_path
            && crate::workspace_host::linked_worktree_root(std::path::Path::new(&cwd)).as_deref()
                == Some(spec.repo_path.as_str())
        {
            return Ok((cwd, None));
        }
        let repos = self
            .inner
            .repos
            .get()
            .ok_or_else(|| EngineError::Other("repos engine not wired".into()))?;
        let worktree = repos
            .create_worktree(std::path::Path::new(&spec.repo_path), &spec.base)
            .await?;
        Ok((worktree.path.clone(), Some(worktree)))
    }

    pub fn purge_chat(&self, chat_id: &str) -> Result<(), EngineError> {
        lock(&self.inner.handles).remove(chat_id);
        Ok(())
    }

    /// The workspace host, once wired (tests may assemble a DocHost without one).
    pub fn workspace(&self) -> Option<&WorkspaceHost> {
        self.inner.workspace.get()
    }

    pub fn device_id(&self) -> &str {
        &self.inner.config.device_id
    }

    /// Open (or return) the chat's doc handle: load the local snapshot (or init fresh)
    /// and start the change-driven task.
    pub fn open(&self, chat_id: &str) -> Result<Arc<ChatDocHandle>, EngineError> {
        if let Some(handle) = lock(&self.inner.handles).get(chat_id) {
            return Ok(handle.clone());
        }
        let doc = match self.inner.store.load_snapshot(chat_id)? {
            Some(bytes) => {
                let raw = loro::LoroDoc::new();
                raw.import(&bytes)
                    .map_err(|e| EngineError::Other(format!("snapshot import failed: {e}")))?;
                SessionDoc::from_doc(raw)
            }
            None => SessionDoc::init(chat_id)?,
        };
        let doc = Arc::new(doc);

        let (changed_tx, changed_rx) = watch::channel(0u64);
        let sub = doc.doc().subscribe_root(Arc::new(move |_diff| {
            changed_tx.send_modify(|v| *v = v.wrapping_add(1));
        }));
        let joined = join_continuation_entries(doc.read_entries()?);
        let (messages_tx, _) = watch::channel(joined);

        let handle = Arc::new(ChatDocHandle {
            chat_id: chat_id.to_string(),
            device_id: self.inner.config.device_id.clone(),
            doc: doc.clone(),
            messages_tx,
            _sub: sub,
        });
        {
            let mut handles = lock(&self.inner.handles);
            if let Some(existing) = handles.get(chat_id) {
                return Ok(existing.clone()); // racing open — keep the first
            }
            handles.insert(chat_id.to_string(), handle.clone());
        }

        tokio::spawn(chat_task(self.clone(), Arc::downgrade(&handle), changed_rx));
        Ok(handle)
    }

    /// Composer path: append an immutable pending command entry (rule 1). Durable by
    /// construction — the change subscription kicks the drain, so a local host executes
    /// immediately and an offline doc simply holds the entry until it syncs.
    pub fn queue_command(
        &self,
        chat_id: &str,
        payload: SessionCommandPayload,
    ) -> Result<String, EngineError> {
        let handle = self.open(chat_id)?;
        let id = new_id();
        let now = now_ms();
        let is_message = matches!(
            &payload,
            SessionCommandPayload::Run { .. } | SessionCommandPayload::Steer { .. }
        );
        let based_on = handle.doc.read_entries()?.last().map(|m| CommandBasedOn {
            turn_id: Some(m.id.clone()),
            frontier: None,
        });
        handle.doc.queue_command(&SessionCommandEntry {
            id: id.clone(),
            payload,
            issued_by: self.inner.config.device_id.clone(),
            issued_at: now,
            based_on,
            expires_at: Some(now + COMMAND_DEFAULT_TTL_MS),
            status: SessionCommandStatus::Pending,
            resolution: None,
        })?;
        if is_message
            && let Some(workspace) = self.workspace()
            && let Ok(Some(chat)) = workspace.chat(chat_id)
            && chat.archived
            && let Err(err) = workspace.set_chat_archived(chat_id, false)
        {
            tracing::warn!(chat = %chat_id, error = %err, "unarchive on send failed");
        }
        Ok(id)
    }

    /// §2.2 writer discipline: we host a chat iff its workspace row's `deviceId` is
    /// ours; a chat with no row is claimable (claim-on-first-command). Without a
    /// wired workspace host (bare-DocHost tests) every open chat is ours — M2's
    /// behavior, now the degenerate case.
    fn is_host(&self, chat_id: &str) -> bool {
        self.workspace().is_none_or(|ws| ws.is_host(chat_id))
    }

    /// Chat-config harness when the workspace row carries one, else the default.
    fn harness_for(&self, chat_id: &str) -> HarnessId {
        self.workspace()
            .and_then(|ws| ws.chat_config(chat_id))
            .map(|config| config.harness)
            .unwrap_or(self.inner.config.default_harness)
    }

    /// Drain pending commands (host-only): evaluate → mark processed BEFORE execute →
    /// execute → write the outcome as the sole outcome writer.
    pub async fn drain_commands(&self, handle: &Arc<ChatDocHandle>) {
        let Some(sessions) = self.inner.sessions.get() else {
            return; // executor not wired yet; the set_sessions kick re-drains
        };
        if !self.is_host(&handle.chat_id) {
            return;
        }
        loop {
            let commands = match handle.doc.read_commands() {
                Ok(commands) => commands,
                Err(err) => {
                    tracing::warn!(chat = %handle.chat_id, error = %err, "command read failed");
                    return;
                }
            };
            let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
            let Some(entry) = commands
                .iter()
                .find(|c| c.status == SessionCommandStatus::Pending)
                .cloned()
            else {
                return;
            };
            let messages = handle.doc.read_entries().unwrap_or_default();
            let current_turn_id = messages.last().map(|m| m.id.clone());
            let turn_is_past = |turn_id: &str| messages.iter().any(|m| m.id == turn_id);
            let disposition = evaluate_command(
                &entry,
                &EvaluationContext {
                    is_processed: &is_processed,
                    now_ms: now_ms(),
                    entries: &commands,
                    current_turn_id: current_turn_id.as_deref(),
                    turn_is_past: &turn_is_past,
                },
            );
            // Mark BEFORE executing: a crash mid-execution must never double-run a
            // command whose side effect may already have happened.
            if let Err(err) = self.inner.store.mark_processed(&entry.id) {
                tracing::error!(chat = %handle.chat_id, error = %err, "processed-ledger write failed; halting drain");
                return;
            }
            match disposition {
                CommandDisposition::Skip => {
                    self.resolve_command(
                        handle,
                        &entry.id,
                        SessionCommandStatus::Rejected,
                        Some("command was already processed"),
                    );
                }
                CommandDisposition::Expired => {
                    self.resolve_command(handle, &entry.id, SessionCommandStatus::Expired, None);
                }
                CommandDisposition::Superseded => {
                    self.resolve_command(handle, &entry.id, SessionCommandStatus::Superseded, None);
                }
                CommandDisposition::Execute => {
                    let (status, resolution) = match self.execute(sessions, handle, &entry).await {
                        Ok(outcome) => outcome,
                        Err(err) => (SessionCommandStatus::Rejected, Some(err.to_string())),
                    };
                    self.resolve_command(handle, &entry.id, status, resolution.as_deref());
                }
            }
        }
    }

    /// Host-only outcome write (ledger rule 2).
    fn resolve_command(
        &self,
        handle: &ChatDocHandle,
        command_id: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) {
        if let Err(err) = handle
            .doc
            .set_command_status(command_id, status, resolution)
        {
            tracing::warn!(
                chat = %handle.chat_id,
                command = %command_id,
                error = %err,
                "command outcome write failed"
            );
        }
    }

    async fn execute(
        &self,
        sessions: &SessionsEngine,
        handle: &Arc<ChatDocHandle>,
        entry: &SessionCommandEntry,
    ) -> Result<(SessionCommandStatus, Option<String>), EngineError> {
        let chat_id = &handle.chat_id;
        match &entry.payload {
            SessionCommandPayload::Run {
                request,
                message_id,
            } => {
                let mut request = request.clone();
                let fresh_worktree = match request.worktree.take() {
                    Some(spec) => {
                        let (cwd, worktree) = self.materialize_worktree(chat_id, &spec).await?;
                        request.cwd = cwd;
                        worktree
                    }
                    None => None,
                };
                // Claim-on-first-command: a run for a chat with no workspace row
                // creates the row under our device id (we are about to host it).
                if let Some(ws) = self.workspace() {
                    ws.claim_chat(chat_id, Some(&request.cwd))?;
                    if let Some(worktree) = &fresh_worktree {
                        if let Err(err) = ws.set_chat_cwd(chat_id, &worktree.path) {
                            tracing::warn!(chat = %chat_id, error = %err, "worktree cwd stamp failed");
                        }
                        if let Err(err) = ws.set_chat_branch(chat_id, &worktree.branch) {
                            tracing::warn!(chat = %chat_id, error = %err, "worktree branch stamp failed");
                        }
                    }
                    if ws.chat_config(chat_id).is_none() {
                        let config = zeron_proto::ChatConfig {
                            harness: request
                                .harness
                                .clone()
                                .unwrap_or_else(|| self.harness_for(chat_id)),
                            model: request.model.clone(),
                            reasoning: request.reasoning,
                            model_options: request.model_options.clone(),
                            sandbox: request.sandbox,
                        };
                        if let Err(err) = ws.set_chat_config(chat_id, &config) {
                            tracing::warn!(chat = %chat_id, error = %err, "run-config backfill failed");
                        }
                    }
                }
                let harness = self.harness_for_request(chat_id, &request);
                sessions
                    .dispatch(chat_id, harness, request, Some(message_id.clone()))
                    .await?;
                Ok((SessionCommandStatus::Applied, None))
            }
            SessionCommandPayload::Steer { prompt, message_id } => {
                match sessions.steer(chat_id, prompt, message_id.clone()).await? {
                    SteerOutcome::Accepted => Ok((SessionCommandStatus::Applied, None)),
                    SteerOutcome::NotSteerable => {
                        // No live steerable run: the durable command still delivers —
                        // run it as the next turn (comet's fallback, executor-side).
                        let request = sessions
                            .last_request(chat_id)
                            .or_else(|| self.request_from_chat_row(chat_id, prompt));
                        let Some(mut request) = request else {
                            return Ok((
                                SessionCommandStatus::Rejected,
                                Some("no live run and no prior run config".into()),
                            ));
                        };
                        request.prompt = prompt.clone();
                        request.resume = None; // dispatch re-derives the harness session
                        request.attachments = Vec::new();
                        sessions
                            .dispatch(
                                chat_id,
                                self.harness_for_request(chat_id, &request),
                                request,
                                message_id.clone(),
                            )
                            .await?;
                        Ok((
                            SessionCommandStatus::Applied,
                            Some("queued as new turn".into()),
                        ))
                    }
                }
            }
            SessionCommandPayload::Interrupt {} => {
                sessions.interrupt(chat_id).await?;
                Ok((SessionCommandStatus::Applied, None))
            }
            SessionCommandPayload::RespondInput {
                request_id,
                answers,
            } => {
                if sessions.respond_input(chat_id, request_id, answers.clone())? {
                    return Ok((SessionCommandStatus::Applied, None));
                }
                let questions = handle.doc.read_entries().ok().and_then(|entries| {
                    entries
                        .iter()
                        .rev()
                        .filter(|e| e.status != Some(MessageStatus::Streaming))
                        .find_map(|e| {
                            e.parts.iter().find_map(|p| match p {
                                MessagePart::Input {
                                    request_id: id,
                                    questions,
                                    resolved: false,
                                    ..
                                } if id == request_id => Some(questions.clone()),
                                _ => None,
                            })
                        })
                });
                let Some(questions) = questions else {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("no pending input request".into()),
                    ));
                };
                let request = sessions
                    .last_request(chat_id)
                    .or_else(|| self.request_from_chat_row(chat_id, ""));
                let Some(mut request) = request else {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("no pending input request and no prior run config".into()),
                    ));
                };
                request.prompt = respond_input_prompt(&questions, answers);
                request.resume = None;
                request.attachments = Vec::new();
                if let Err(err) = handle.doc.resolve_input(request_id) {
                    tracing::warn!(
                        chat = %chat_id,
                        request = %request_id,
                        error = %err,
                        "orphaned input resolve failed"
                    );
                }
                sessions
                    .dispatch(
                        chat_id,
                        self.harness_for_request(chat_id, &request),
                        request,
                        None,
                    )
                    .await?;
                Ok((
                    SessionCommandStatus::Applied,
                    Some("answered as new turn".into()),
                ))
            }
        }
    }

    fn save_snapshot(&self, handle: &ChatDocHandle) {
        match handle.doc.export_snapshot() {
            Ok(bytes) => {
                if let Err(err) = self.inner.store.save_snapshot(&handle.chat_id, &bytes) {
                    tracing::warn!(chat = %handle.chat_id, error = %err, "snapshot save failed");
                }
            }
            Err(err) => {
                tracing::warn!(chat = %handle.chat_id, error = %err, "snapshot export failed");
            }
        }
    }

    /// Persist every open doc now (shutdown path; bypasses the debounce).
    pub fn flush_all(&self) {
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            self.save_snapshot(&handle);
        }
    }
}

/// Build a natural-language prompt when an input question outlives its run.
pub fn respond_input_prompt(
    questions: &[UserInputQuestion],
    answers: &[UserInputAnswer],
) -> String {
    let mut lines = vec!["Answering your earlier question:".to_string()];
    for answer in answers {
        let picked = answer.labels.join(", ");
        let question = questions
            .iter()
            .find(|question| question.id == answer.question_id)
            .map(|question| question.question.trim())
            .filter(|question| !question.is_empty());
        match question {
            Some(question) => lines.push(format!("{question} — {picked}")),
            None => lines.push(picked),
        }
    }
    lines.join("\n")
}

/// Per-chat background task: reacts to local doc changes
/// by re-publishing the transcript watch, draining commands, and debouncing snapshots.
/// Holds only a weak handle so a dropped host tears the task down.
async fn chat_task(host: DocHost, weak: Weak<ChatDocHandle>, mut changed_rx: watch::Receiver<u64>) {
    // Initial pass: the snapshot may already carry pending commands.
    {
        let Some(handle) = weak.upgrade() else { return };
        handle.publish_messages();
        host.drain_commands(&handle).await;
    }
    let mut save_deadline: Option<tokio::time::Instant> = None;
    loop {
        let sleep_until = save_deadline.unwrap_or_else(tokio::time::Instant::now);
        tokio::select! {
            changed = changed_rx.changed() => {
                if changed.is_err() {
                    break; // doc handle (and its change sender) is gone
                }
                let Some(handle) = weak.upgrade() else { break };
                handle.publish_messages();
                host.drain_commands(&handle).await;
                if save_deadline.is_none() {
                    save_deadline = Some(
                        tokio::time::Instant::now()
                            + std::time::Duration::from_millis(SNAPSHOT_DEBOUNCE_MS),
                    );
                }
            }
            _ = tokio::time::sleep_until(sleep_until), if save_deadline.is_some() => {
                save_deadline = None;
                let Some(handle) = weak.upgrade() else { break };
                host.save_snapshot(&handle);
            }
        }
    }
}
