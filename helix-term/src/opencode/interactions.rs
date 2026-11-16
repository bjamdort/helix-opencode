//! Pending user actions. No decision is inferred from focus or reconnection.
use super::*;
use serde_json::Map;

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct PermissionRequest {
    pub id: String,
    #[serde(rename = "sessionID", default)]
    pub session_id: String,
    pub action: String,
    #[serde(default)]
    pub resources: Vec<String>,
    pub message: Option<String>,
    pub metadata: Option<Value>,
    pub source: Option<Value>,
    pub save: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct Form {
    pub id: String,
    #[serde(rename = "sessionID", default)]
    pub session_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub fields: Vec<FormField>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct FormOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct Condition {
    pub key: String,
    pub op: String,
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct FormField {
    pub key: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub options: Vec<FormOption>,
    #[serde(default)]
    pub custom: bool,
    #[serde(default)]
    pub when: Vec<Condition>,
    pub default: Option<Value>,
    pub url: Option<String>,
    #[serde(flatten)]
    pub constraints: Map<String, Value>,
}

impl FormField {
    pub fn active(&self, answer: &Map<String, Value>) -> bool {
        self.when.iter().all(|condition| {
            let Some(value) = answer.get(&condition.key) else {
                return false;
            };
            let equal = |value: &Value| {
                if value.is_number() && condition.value.is_number() {
                    value.as_f64() == condition.value.as_f64()
                } else {
                    value == &condition.value
                }
            };
            let matches = value
                .as_array()
                .map_or_else(|| equal(value), |values| values.iter().any(equal));
            match condition.op.as_str() {
                "eq" => matches,
                "neq" => !matches,
                _ => false,
            }
        })
    }

    pub fn parse(&self, input: &str) -> Result<Value> {
        let value = match self.kind.as_str() {
            "string" => Value::String(input.to_owned()),
            "number" | "integer" | "boolean" | "multiselect" => serde_json::from_str(input)
                .context("Enter a JSON number, boolean, or array of strings")?,
            _ => bail!("This field cannot be edited as text"),
        };
        self.validate(&value)?;
        Ok(value)
    }

    fn validate(&self, value: &Value) -> Result<()> {
        let valid_type = match self.kind.as_str() {
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "external" => value == &Value::Bool(true),
            "number" => value.as_f64().is_some_and(f64::is_finite),
            "integer" => value
                .as_f64()
                .is_some_and(|n| n.is_finite() && n.fract() == 0.0),
            "multiselect" => value
                .as_array()
                .is_some_and(|v| v.iter().all(Value::is_string)),
            _ => false,
        };
        anyhow::ensure!(valid_type, "Invalid value type for {}", self.key);
        let length = value
            .as_str()
            .map(|s| s.encode_utf16().count())
            .or_else(|| value.as_array().map(Vec::len));
        if let Some(length) = length {
            anyhow::ensure!(!self.required || length > 0, "{} is required", self.key);
            let (min, max) = if self.kind == "multiselect" {
                ("minItems", "maxItems")
            } else {
                ("minLength", "maxLength")
            };
            if let Some(min) = self.constraints.get(min).and_then(Value::as_u64) {
                anyhow::ensure!(
                    length as u64 >= min,
                    "{} needs at least {min} items/characters",
                    self.key
                );
            }
            if let Some(max) = self.constraints.get(max).and_then(Value::as_u64) {
                anyhow::ensure!(
                    length as u64 <= max,
                    "{} allows at most {max} items/characters",
                    self.key
                );
            }
        }
        if let Some(number) = value.as_f64() {
            if let Some(min) = self.constraints.get("minimum").and_then(Value::as_f64) {
                anyhow::ensure!(number >= min, "{} must be at least {min}", self.key);
            }
            if let Some(max) = self.constraints.get("maximum").and_then(Value::as_f64) {
                anyhow::ensure!(number <= max, "{} must be at most {max}", self.key);
            }
        }
        if !self.custom && !self.options.is_empty() {
            let allowed = |value: &Value| {
                self.options
                    .iter()
                    .any(|option| value.as_str() == Some(&option.value))
            };
            anyhow::ensure!(
                value
                    .as_array()
                    .map_or_else(|| allowed(value), |values| values.iter().all(allowed)),
                "{} must use one of the listed options",
                self.key
            );
        }
        // The server remains authoritative for regex and format validation.
        Ok(())
    }
}

impl Form {
    /// Remove inactive answers in field order so conditional visibility cascades.
    pub fn normalize(&self, draft: &Map<String, Value>) -> Map<String, Value> {
        let mut answer = Map::new();
        for field in &self.fields {
            if field.active(&answer) {
                if let Some(value) = draft.get(&field.key).or(field.default.as_ref()) {
                    answer.insert(field.key.clone(), value.clone());
                }
            }
        }
        answer
    }

    pub fn validate(&self, answer: &Map<String, Value>) -> Result<()> {
        for field in &self.fields {
            if !field.active(answer) {
                continue;
            }
            if let Some(value) = answer.get(&field.key) {
                field.validate(value)?;
            } else {
                anyhow::ensure!(
                    !field.required && field.kind != "external",
                    "{} is required",
                    field.key
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct FileChange {
    pub file: String,
    pub patch: String,
    pub additions: u64,
    pub deletions: u64,
    pub status: String,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum PermissionDecision {
    Once,
    Reject,
}

impl PermissionDecision {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Reject => "reject",
        }
    }
}

pub(super) enum ReplyStatus {
    Applied,
    Settled,
}

#[derive(Clone)]
pub(crate) struct InteractionHandle {
    pub state: Arc<Mutex<State>>,
    requests: mpsc::Sender<Request>,
}

impl InteractionHandle {
    pub fn permission(
        &self,
        expected: PermissionRequest,
        decision: PermissionDecision,
    ) -> Result<()> {
        let id = expected.id.clone();
        self.enqueue(&id, Request::Permission { expected, decision })
    }

    pub fn form(&self, expected: Form, answer: Option<Map<String, Value>>) -> Result<()> {
        let answer = answer.map(|answer| expected.normalize(&answer));
        if let Some(answer) = &answer {
            expected.validate(answer)?;
        }
        let id = expected.id.clone();
        self.enqueue(&id, Request::Form { expected, answer })
    }

    fn enqueue(&self, id: &str, request: Request) -> Result<()> {
        let mut state = self.state.lock();
        anyhow::ensure!(
            state.connected,
            "Disconnected. Close this view and press r to reconnect"
        );
        match &request {
            Request::Permission { expected, .. } => anyhow::ensure!(
                state.permissions.get(id) == Some(expected),
                "This permission was settled or changed; refresh the list"
            ),
            Request::Form { expected, .. } => anyhow::ensure!(
                state.forms.get(id) == Some(expected),
                "This question was settled or changed; refresh the list"
            ),
            _ => (),
        }
        anyhow::ensure!(
            !state.resolving.contains(id),
            "This request is already being submitted"
        );
        self.requests.try_send(request).map_err(|_| {
            anyhow::anyhow!("Request worker is unavailable; reconnect and reopen this view")
        })?;
        state.resolving.insert(id.to_owned());
        state.interaction_status = Some("Submitting…".into());
        Ok(())
    }

    pub fn refresh(&self) -> Result<()> {
        self.requests
            .try_send(Request::RefreshRequests)
            .map_err(|_| anyhow::anyhow!("Reconnect and reopen this view"))?;
        Ok(())
    }

    pub fn review(&self) -> Result<()> {
        let mut state = self.state.lock();
        anyhow::ensure!(state.connected, "Reconnect before reviewing changes");
        self.requests
            .try_send(Request::Review)
            .map_err(|_| anyhow::anyhow!("Reconnect and reopen this view"))?;
        state.review_loading = true;
        state.review = None;
        state.review_error = None;
        Ok(())
    }
}

impl Connection {
    pub fn interactions(&self) -> InteractionHandle {
        InteractionHandle {
            state: self.state.clone(),
            requests: self.requests.clone(),
        }
    }
}

pub(super) async fn refresh(api: &Api, id: &str, state: &Mutex<State>) -> Result<()> {
    let permissions = api.permissions(id).await?;
    let forms = api.forms(id).await?;
    let mut state = state.lock();
    state.permissions = permissions
        .into_iter()
        .filter(|p| p.session_id == id)
        .map(|p| (p.id.clone(), p))
        .collect();
    state.forms = forms
        .into_iter()
        .filter(|f| f.session_id == id)
        .map(|f| (f.id.clone(), f))
        .collect();
    state.rebuild_notices();
    Ok(())
}

pub(super) async fn permission_reply(
    api: &Api,
    session: &str,
    expected: &PermissionRequest,
    decision: PermissionDecision,
) -> Result<ReplyStatus> {
    anyhow::ensure!(
        valid_id(&expected.id, "per") && expected.session_id == session,
        "Permission belongs to a different session"
    );
    let current = api
        .permissions(session)
        .await?
        .into_iter()
        .find(|p| p.id == expected.id);
    let Some(current) = current else {
        return Ok(ReplyStatus::Settled);
    };
    anyhow::ensure!(
        current == *expected,
        "Permission details changed. Refresh and inspect the request again"
    );
    api.permission_reply(session, &expected.id, decision).await
}

pub(super) async fn form_reply(
    api: &Api,
    session: &str,
    expected: &Form,
    answer: Option<&Map<String, Value>>,
) -> Result<ReplyStatus> {
    anyhow::ensure!(
        valid_id(&expected.id, "frm_") && expected.session_id == session,
        "Form belongs to a different session"
    );
    let current = api
        .forms(session)
        .await?
        .into_iter()
        .find(|f| f.id == expected.id);
    let Some(current) = current else {
        return Ok(ReplyStatus::Settled);
    };
    anyhow::ensure!(
        current == *expected,
        "Question details changed. Refresh and inspect it again"
    );
    api.form_reply(session, &expected.id, answer).await
}
