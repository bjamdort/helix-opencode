//! Native OpenCode V2 session client and transcript state.
mod client;
pub(crate) mod interactions;
#[cfg(test)]
mod live_tests;
pub(crate) mod models;
pub(crate) mod snippets;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use tokio::{sync::mpsc, task::JoinHandle};

use client::{Api, Endpoint, Event, EventDecoder};

#[derive(Clone, Debug, Default, Deserialize, serde::Serialize, PartialEq, Eq)]
pub(crate) struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider: String,
    #[serde(rename = "id")]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl ModelRef {
    pub fn label(&self) -> String {
        let mut label = format!("{}/{}", self.provider, self.model);
        if let Some(variant) = self
            .variant
            .as_deref()
            .filter(|variant| *variant != "default")
        {
            label.push('#');
            label.push_str(variant);
        }
        label
    }

    pub fn same_selection(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.model == other.model
            && self.variant.as_deref().unwrap_or("default")
                == other.variant.as_deref().unwrap_or("default")
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct Times {
    pub busy: Option<u64>,
    pub idle: Option<u64>,
    pub completed: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Location {
    pub directory: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Session {
    pub id: String,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub model: Option<ModelRef>,
    pub location: Location,
    pub time: Times,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct ToolState {
    pub status: String,
    pub input: Option<Value>,
    #[serde(default)]
    pub content: Vec<Value>,
    pub error: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct Part {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub state: Option<ToolState>,
    #[serde(skip)]
    complete: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct Message {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub content: Vec<Part>,
    pub agent: Option<String>,
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub time: Times,
    pub error: Option<Value>,
    #[serde(default)]
    pub files: Vec<Value>,
    pub metadata: Option<Value>,
}

#[derive(Clone, Debug)]
pub(crate) struct Pending {
    pub id: String,
    pub text: String,
    pub accepted: bool,
    pub failed: bool,
    pub snippets: Vec<snippets::Snippet>,
    pub files: Vec<Value>,
    pub metadata: Option<Value>,
}

#[derive(Deserialize)]
pub(super) struct InboxEntry {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    payload: InboxPayload,
}

#[derive(Deserialize)]
struct InboxPayload {
    #[serde(default)]
    text: String,
    #[serde(default)]
    files: Vec<Value>,
    metadata: Option<Value>,
}

#[derive(Default)]
pub(crate) struct State {
    pub session: Option<Session>,
    pub connected: bool,
    pub busy: bool,
    pub status: String,
    pub error: Option<String>,
    pub messages: Vec<Message>,
    pub pending: Vec<Pending>,
    pub notices: HashMap<String, String>,
    pub permissions: BTreeMap<String, interactions::PermissionRequest>,
    pub forms: BTreeMap<String, interactions::Form>,
    pub resolving: HashSet<String>,
    pub interaction_status: Option<String>,
    pub review: Option<Vec<interactions::FileChange>>,
    pub review_loading: bool,
    pub review_error: Option<String>,
    #[cfg(test)]
    text_deltas: usize,
}

impl State {
    fn rebuild_notices(&mut self) {
        self.notices = self
            .permissions
            .values()
            .map(|p| {
                (
                    p.id.clone(),
                    format!("Approval needed: {}. Press p to inspect.", p.action),
                )
            })
            .chain(self.forms.values().map(|f| {
                (
                    f.id.clone(),
                    format!("Question: {}. Press p to answer.", f.title),
                )
            }))
            .collect();
    }
    fn upsert(&mut self, message: Message) {
        self.pending.retain(|pending| pending.id != message.id);
        if let Some(existing) = self
            .messages
            .iter_mut()
            .find(|existing| existing.id == message.id)
        {
            *existing = message;
        } else {
            self.messages.push(message);
        }
        if self.messages.len() > 200 {
            self.messages.remove(0);
        }
    }

    fn assistant(&mut self, id: &str) -> &mut Message {
        if let Some(index) = self.messages.iter().position(|message| message.id == id) {
            return &mut self.messages[index];
        }
        self.messages.push(Message {
            id: id.into(),
            kind: "assistant".into(),
            ..Message::default()
        });
        self.messages.last_mut().unwrap()
    }

    fn apply(&mut self, event: Event) -> Result<()> {
        if event.kind == "session.model.selected" {
            let model: ModelRef = serde_json::from_value(
                event
                    .data
                    .get("model")
                    .cloned()
                    .context("Missing selected model")?,
            )?;
            if let Some(session) = &mut self.session {
                session.model = Some(model);
            }
            return Ok(());
        }
        if event.kind == "permission.asked" {
            let permission: interactions::PermissionRequest = serde_json::from_value(event.data)?;
            self.permissions.insert(permission.id.clone(), permission);
            self.rebuild_notices();
            return Ok(());
        }
        if event.kind == "form.created" {
            let form: interactions::Form =
                serde_json::from_value(event.data.get("form").cloned().context("Missing form")?)?;
            self.forms.insert(form.id.clone(), form);
            self.rebuild_notices();
            return Ok(());
        }
        // Other session events can reuse field names with unrelated shapes
        // (instructions.updated has an `agent` map). Do not decode them as chat.
        if !matches!(
            event.kind.as_str(),
            "session.step.started"
                | "session.execution.started"
                | "session.execution.succeeded"
                | "session.execution.interrupted"
                | "session.execution.failed"
                | "session.text.started"
                | "session.text.delta"
                | "session.text.ended"
                | "session.reasoning.started"
                | "session.reasoning.delta"
                | "session.reasoning.ended"
                | "session.tool.input.started"
                | "session.tool.called"
                | "session.tool.success"
                | "session.tool.failed"
                | "permission.asked"
                | "permission.replied"
                | "form.created"
                | "form.replied"
                | "form.cancelled"
        ) {
            return Ok(());
        }
        let data: EventData = serde_json::from_value(event.data)
            .with_context(|| format!("Unexpected OpenCode {} event", event.kind))?;
        match event.kind.as_str() {
            "session.step.started" => {
                let message = self.assistant(
                    data.assistant_message_id
                        .as_deref()
                        .context("Missing assistant message ID")?,
                );
                message.agent = data.agent;
                message.model = data.model;
            }
            "session.execution.started" => {
                self.busy = true;
                self.status = "Running".into();
                self.error = None;
            }
            "session.execution.succeeded" | "session.execution.interrupted" => {
                self.busy = false;
                self.status = "Ready".into();
            }
            "session.execution.failed" => {
                self.busy = false;
                self.status = "Failed".into();
                self.error = Some(error_summary(data.error.as_ref()));
            }
            kind if kind.starts_with("session.text.") || kind.starts_with("session.reasoning.") => {
                let kind_name = if kind.starts_with("session.text.") {
                    "text"
                } else {
                    "reasoning"
                };
                let ordinal = data.ordinal.context("Missing content ordinal")?;
                if ordinal > 4096 {
                    bail!("OpenCode content ordinal exceeds the limit");
                }
                let message = self.assistant(
                    data.assistant_message_id
                        .as_deref()
                        .context("Missing assistant message ID")?,
                );
                // Ordinals are per content kind, not positions in the mixed part array.
                while message
                    .content
                    .iter()
                    .filter(|part| part.kind == kind_name)
                    .count()
                    <= ordinal
                {
                    message.content.push(Part {
                        kind: kind_name.into(),
                        ..Part::default()
                    });
                }
                let part = message
                    .content
                    .iter_mut()
                    .filter(|part| part.kind == kind_name)
                    .nth(ordinal)
                    .unwrap();
                if kind.ends_with(".ended") {
                    part.text = data.text.context("Missing completed text")?;
                    part.complete = true;
                } else if kind.ends_with(".delta") && !part.complete {
                    part.text
                        .push_str(&data.delta.context("Missing text delta")?);
                    #[cfg(test)]
                    if kind_name == "text" {
                        self.text_deltas += 1;
                    }
                }
            }
            "session.tool.input.started"
            | "session.tool.called"
            | "session.tool.success"
            | "session.tool.failed" => {
                let message = self.assistant(
                    data.assistant_message_id
                        .as_deref()
                        .context("Missing assistant message ID")?,
                );
                let id = data.id.context("Missing tool ID")?;
                let index = match message
                    .content
                    .iter()
                    .position(|part| part.kind == "tool" && part.id == id)
                {
                    Some(index) => index,
                    None => {
                        message.content.push(Part {
                            kind: "tool".into(),
                            id,
                            ..Part::default()
                        });
                        message.content.len() - 1
                    }
                };
                let part = &mut message.content[index];
                if let Some(name) = data.name {
                    part.name = name;
                }
                let tool = part.state.get_or_insert_with(ToolState::default);
                tool.status = match event.kind.as_str() {
                    "session.tool.success" => "completed",
                    "session.tool.failed" => "error",
                    "session.tool.called" => "running",
                    _ => "streaming",
                }
                .into();
                if let Some(input) = data.input {
                    tool.input = Some(input);
                }
                if let Some(content) = data.content {
                    tool.content = content;
                }
                if let Some(error) = data.error {
                    tool.error = Some(error);
                }
            }
            "permission.replied" => {
                self.permissions
                    .remove(&data.request_id.unwrap_or_default());
                self.rebuild_notices();
            }
            "form.replied" | "form.cancelled" => {
                self.forms.remove(&data.id.unwrap_or_default());
                self.rebuild_notices();
            }
            _ => (),
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventData {
    #[serde(rename = "assistantMessageID")]
    assistant_message_id: Option<String>,
    ordinal: Option<usize>,
    text: Option<String>,
    delta: Option<String>,
    id: Option<String>,
    #[serde(rename = "requestID")]
    request_id: Option<String>,
    name: Option<String>,
    agent: Option<String>,
    model: Option<ModelRef>,
    error: Option<Value>,
    input: Option<Value>,
    content: Option<Vec<Value>>,
}

pub(crate) fn error_summary(error: Option<&Value>) -> String {
    error
        .and_then(|error| {
            error
                .get("message")
                .or_else(|| error.get("data").and_then(|data| data.get("message")))
        })
        .and_then(Value::as_str)
        .unwrap_or("OpenCode execution failed; inspect the service logs")
        .to_owned()
}

enum Request {
    Models(tokio::sync::oneshot::Sender<Result<Vec<models::Model>>>),
    SwitchModel {
        model: ModelRef,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    Send {
        id: String,
        text: String,
        snippets: Vec<snippets::Snippet>,
    },
    Interrupt,
    Permission {
        expected: interactions::PermissionRequest,
        decision: interactions::PermissionDecision,
    },
    Form {
        expected: interactions::Form,
        answer: Option<serde_json::Map<String, Value>>,
    },
    RefreshRequests,
    Review,
}

pub(crate) struct Connection {
    pub state: Arc<Mutex<State>>,
    pub id: String,
    directory: PathBuf,
    attach: bool,
    requests: mpsc::Sender<Request>,
    task: JoinHandle<()>,
}

impl Connection {
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let (requests, rx) = mpsc::channel(8);
        Self {
            state: Arc::new(Mutex::new(State::default())),
            id: "ses_test".into(),
            directory: "/test".into(),
            attach: true,
            requests,
            task: tokio::spawn(async move {
                let _rx = rx;
                std::future::pending().await
            }),
        }
    }
    pub fn new(directory: PathBuf, session: Option<String>) -> Self {
        let attach = session.is_some();
        let id = session.unwrap_or_else(|| new_id("ses"));
        let state = Arc::new(Mutex::new(State::default()));
        let (requests, task) = start(directory.clone(), id.clone(), attach, state.clone());
        Self {
            state,
            id,
            directory,
            attach,
            requests,
            task,
        }
    }

    pub fn reconnect(&mut self) {
        self.task.abort();
        let attach = self.attach || self.state.lock().session.is_some();
        (self.requests, self.task) = start(
            self.directory.clone(),
            self.id.clone(),
            attach,
            self.state.clone(),
        );
    }

    pub fn send(&mut self, text: String) -> Result<()> {
        self.send_snippets(text, Vec::new())
    }

    pub fn send_snippets(&mut self, text: String, snippets: Vec<snippets::Snippet>) -> Result<()> {
        if text.trim().is_empty()
            && snippets
                .iter()
                .all(|snippet| snippet.comment.trim().is_empty())
        {
            bail!("Message is empty");
        }
        let mut state = self.state.lock();
        if !state.connected {
            bail!("OpenCode is not connected; press r to reconnect");
        }
        let id = new_id("msg");
        self.requests
            .try_send(Request::Send {
                id: id.clone(),
                text: text.clone(),
                snippets: snippets.clone(),
            })
            .map_err(|_| anyhow::anyhow!("OpenCode request queue is unavailable"))?;
        state.pending.push(Pending {
            id,
            text,
            accepted: false,
            failed: false,
            snippets,
            files: Vec::new(),
            metadata: None,
        });
        Ok(())
    }

    pub fn interrupt(&self) -> Result<()> {
        self.requests
            .try_send(Request::Interrupt)
            .map_err(|_| anyhow::anyhow!("OpenCode is disconnected"))?;
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn start(
    directory: PathBuf,
    id: String,
    attach: bool,
    state: Arc<Mutex<State>>,
) -> (mpsc::Sender<Request>, JoinHandle<()>) {
    {
        let mut state = state.lock();
        state.connected = false;
        state.status = "Connecting…".into();
        state.error = None;
        state.resolving.clear();
    }
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        if let Err(error) = run(directory, &id, attach, state.clone(), rx).await {
            let mut state = state.lock();
            state.connected = false;
            state.status = "Disconnected · r to reconnect".into();
            state.error = Some(format!("{error:#}"));
            helix_event::request_redraw();
        }
    });
    (tx, task)
}

async fn snapshot(api: &Api, id: &str, shared: &Mutex<State>) -> Result<()> {
    let session = api.session(id).await?;
    let mut messages = api.messages(id).await?;
    let inbox = api.inbox(id).await?;
    interactions::refresh(api, id, shared).await?;
    for message in &mut messages {
        for part in &mut message.content {
            part.complete = message.time.completed.is_some() || !part.text.is_empty();
        }
    }
    let mut state = shared.lock();
    state.busy = session
        .time
        .busy
        .is_some_and(|busy| busy > session.time.idle.unwrap_or(0));
    state.status = if state.busy { "Running" } else { "Ready" }.into();
    state
        .pending
        .retain(|pending| !messages.iter().any(|message| message.id == pending.id));
    for input in inbox.into_iter().filter(|input| input.kind == "user") {
        if let Some(pending) = state
            .pending
            .iter_mut()
            .find(|pending| pending.id == input.id)
        {
            pending.accepted = true;
            pending.failed = false;
            pending.files = input.payload.files;
            pending.metadata = input.payload.metadata;
        } else {
            state.pending.push(Pending {
                id: input.id,
                text: input
                    .payload
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("displayText"))
                    .and_then(Value::as_str)
                    .unwrap_or(&input.payload.text)
                    .to_owned(),
                accepted: true,
                failed: false,
                snippets: Vec::new(),
                files: input.payload.files,
                metadata: input.payload.metadata,
            });
        }
    }
    state.messages = messages;
    state.session = Some(session);
    Ok(())
}

async fn run(
    directory: PathBuf,
    id: &str,
    attach: bool,
    state: Arc<Mutex<State>>,
    requests: mpsc::Receiver<Request>,
) -> Result<()> {
    let api = Api::connect(Endpoint::discover().await?).await?;
    run_with_api(api, directory, id, attach, state, requests).await
}

async fn run_with_api(
    api: Api,
    directory: PathBuf,
    id: &str,
    attach: bool,
    state: Arc<Mutex<State>>,
    mut requests: mpsc::Receiver<Request>,
) -> Result<()> {
    if !attach {
        let session = api.create(id, &directory).await?;
        state.lock().session = Some(session);
    }
    // Subscribe before reading projections: events after subscription remain
    // queued, while completed text boundaries replace rather than append content.
    let mut events = api.events().await?;
    snapshot(&api, id, &state).await?;
    state.lock().connected = true;
    helix_event::request_redraw();
    let mut decoder = EventDecoder::default();
    loop {
        tokio::select! {
            request = requests.recv() => {
                let Some(request) = request else { return Ok(()); };
                match request {
                    Request::Models(reply) => {
                        let directory = state.lock().session.as_ref().map(|session| session.location.directory.clone());
                        let result = match directory {
                            Some(directory) => api.models(&directory).await,
                            None => Err(anyhow::anyhow!("No connected OpenCode session")),
                        };
                        let _ = reply.send(result);
                    }
                    Request::SwitchModel { model, reply } => {
                        if reply.is_closed() { continue; }
                        let result = api.switch_model(id, &model).await;
                        let result = match result {
                            Ok(()) => match api.session(id).await {
                                Ok(session) => { state.lock().session = Some(session); Ok(()) }
                                Err(error) => Err(error.context("Model changed, but session refresh failed; reconnect to check")),
                            },
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(result);
                    }
                    Request::Send { id: message_id, text, snippets } => {
                        let result = api.send(id, &message_id, &text, &snippets).await;
                        let mut state = state.lock();
                        if let Some(pending) = state.pending.iter_mut().find(|pending| pending.id == message_id) {
                            pending.accepted = result.is_ok(); pending.failed = result.is_err();
                        }
                        if let Err(error) = result {
                            state.error = Some(format!("{error:#}. Delivery may have occurred; reconnect to check before retrying."));
                        }
                    }
                    Request::Interrupt => {
                        if let Err(error) = api.interrupt(id).await { state.lock().error = Some(format!("{error:#}")); }
                    }
                    Request::Permission { expected, decision } => {
                        let result = interactions::permission_reply(&api, id, &expected, decision).await;
                        finish_interaction(&api, id, &expected.id, result, &state).await;
                    }
                    Request::Form { expected, answer } => {
                        let result = interactions::form_reply(&api, id, &expected, answer.as_ref()).await;
                        finish_interaction(&api, id, &expected.id, result, &state).await;
                    }
                    Request::RefreshRequests => {
                        if let Err(error) = interactions::refresh(&api, id, &state).await {
                            state.lock().interaction_status = Some(format!("Refresh failed: {error:#}"));
                        }
                    }
                    Request::Review => {
                        let result = api.changes(id).await;
                        let mut state = state.lock();
                        state.review_loading = false;
                        match result {
                            Ok(files) => state.review = Some(files),
                            Err(error) => state.review_error = Some(format!("{error:#}")),
                        }
                    }
                }
                helix_event::request_redraw();
            }
            chunk = tokio::time::timeout(Duration::from_secs(90), events.chunk()) => {
                let chunk = chunk.context("OpenCode event stream timed out")?
                    .context("OpenCode event stream failed")?.context("OpenCode event stream closed")?;
                for event in decoder.push(&chunk)? {
                    let event_session = event.data.get("sessionID").or_else(|| event.data.get("form").and_then(|form| form.get("sessionID"))).and_then(Value::as_str);
                    if event_session != Some(id) { continue; }
                    if event.kind == "session.inbox.delivered" {
                        if let Some(message_id) = event.data.get("inboxID").and_then(Value::as_str) {
                            if valid_id(message_id, "msg") {
                                if let Some(message) = api.message(id, message_id).await? { state.lock().upsert(message); }
                            }
                        }
                    } else {
                        let terminal = matches!(event.kind.as_str(), "session.execution.succeeded" | "session.execution.failed" | "session.execution.interrupted");
                        state.lock().apply(event)?;
                        if terminal { snapshot(&api, id, &state).await?; }
                    }
                }
                helix_event::request_redraw();
            }
        }
    }
}

async fn finish_interaction(
    api: &Api,
    session: &str,
    id: &str,
    result: Result<interactions::ReplyStatus>,
    shared: &Mutex<State>,
) {
    {
        let mut state = shared.lock();
        state.resolving.remove(id);
        state.interaction_status = Some(match result {
            Ok(interactions::ReplyStatus::Applied) => "Response submitted.".into(),
            Ok(interactions::ReplyStatus::Settled) => {
                "This request is already settled or no longer available.".into()
            }
            Err(error) => format!("Response not confirmed: {error:#}"),
        });
    }
    if let Err(error) = interactions::refresh(api, session, shared).await {
        shared.lock().interaction_status = Some(format!(
            "Refresh failed: {error:#}. Reconnect before trying again."
        ));
    }
}

pub(crate) fn valid_id(id: &str, prefix: &str) -> bool {
    id.starts_with(prefix)
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn new_id(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{prefix}_helix_{time:x}_{:x}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
