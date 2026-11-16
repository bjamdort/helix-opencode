//! OpenCode V2 HTTP transport, following packages/protocol/openapi.json.
use std::{path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};
use reqwest::{Client, Method, Response, Url};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};

use super::{
    interactions::{FileChange, Form, PermissionDecision, PermissionRequest, ReplyStatus},
    InboxEntry, Message, Session,
};

#[derive(Deserialize)]
struct Registration {
    url: String,
    pid: u32,
    version: Option<String>,
    password: Option<String>,
}

// No Debug: authentication details must not reach logs or UI errors.
pub(super) struct Endpoint {
    url: Url,
    password: Option<String>,
    identity: Option<(u32, Option<String>)>,
}

impl Endpoint {
    pub async fn discover() -> Result<Self> {
        if let Ok(url) = std::env::var("HELIX_OPENCODE_URL") {
            return Self::new(&url, std::env::var("HELIX_OPENCODE_PASSWORD").ok(), None);
        }
        let file = match std::env::var_os("HELIX_OPENCODE_SERVICE_FILE") {
            Some(file) => PathBuf::from(file),
            None => {
                let state = std::env::var_os("XDG_STATE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or(helix_stdx::path::home_dir()?.join(".local/state"));
                state.join("opencode/service.json")
            }
        };
        let bytes = tokio::fs::read(file).await
            .context("Cannot read OpenCode service registration. Start OpenCode first, or set HELIX_OPENCODE_URL")?;
        let registration: Registration =
            serde_json::from_slice(&bytes).context("Invalid OpenCode service registration")?;
        Self::new(
            &registration.url,
            registration.password,
            Some((registration.pid, registration.version)),
        )
    }

    pub(super) fn new(
        url: &str,
        password: Option<String>,
        identity: Option<(u32, Option<String>)>,
    ) -> Result<Self> {
        let url = Url::parse(url).map_err(|_| anyhow::anyhow!("Invalid OpenCode server URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            bail!(
                "OpenCode URL must be an HTTP(S) server origin without credentials, query, or path"
            );
        }
        Ok(Self {
            url,
            password,
            identity,
        })
    }
}

#[derive(Deserialize)]
pub(super) struct Data<T> {
    pub data: T,
}

#[derive(Deserialize)]
struct Info {
    version: String,
    pid: u32,
}

pub(super) struct Api {
    http: Client,
    endpoint: Endpoint,
}

impl Api {
    pub async fn models(&self, directory: &std::path::Path) -> Result<Vec<super::models::Model>> {
        let response = self
            .request(Method::GET, "api/model")?
            .query(&[("location[directory]", directory.to_string_lossy().as_ref())])
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Cannot load OpenCode models"))?;
        Ok(Self::checked(response)
            .await?
            .json::<Data<Vec<super::models::Model>>>()
            .await
            .context("Unexpected OpenCode model catalog")?
            .data)
    }

    pub async fn switch_model(&self, session: &str, model: &super::ModelRef) -> Result<()> {
        let response = self
            .request(Method::POST, &format!("api/session/{session}/model"))?
            .timeout(Duration::from_secs(20))
            .json(&json!({"model": model}))
            .send()
            .await
            .map_err(|_| {
                anyhow::anyhow!("Model change not confirmed; reconnect to check before retrying")
            })?;
        // The V2 endpoint returns 204; do not attempt JSON decoding.
        Self::checked(response).await?;
        Ok(())
    }
    pub async fn connect(endpoint: Endpoint) -> Result<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let api = Self { http, endpoint };
        let info: Info = api.json(Method::GET, "api/info", None).await?;
        if let Some((pid, version)) = &api.endpoint.identity {
            if info.pid != *pid
                || version
                    .as_ref()
                    .is_some_and(|version| version != &info.version)
            {
                bail!("OpenCode service registration is stale; check `opencode service status`");
            }
        }
        Ok(api)
    }

    fn request(&self, method: Method, path: &str) -> Result<reqwest::RequestBuilder> {
        let mut request = self.http.request(method, self.endpoint.url.join(path)?);
        if let Some(password) = &self.endpoint.password {
            request = request.basic_auth("opencode", Some(password));
        }
        Ok(request)
    }

    async fn checked(response: Response) -> Result<Response> {
        match response.status().as_u16() {
            200..=299 => Ok(response),
            401 | 403 => bail!("OpenCode authentication failed. Check the service registration or HELIX_OPENCODE_PASSWORD"),
            404 => bail!("OpenCode resource not found. Check the session ID and the service's V2 /api support"),
            code => bail!("OpenCode request failed (HTTP {code})"),
        }
    }

    pub(super) async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T> {
        let context = format!("{} {}", method, path.split('?').next().unwrap_or(path));
        let mut request = self.request(method, path)?.timeout(Duration::from_secs(20));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Cannot reach the OpenCode service for {context}"))?;
        Self::checked(response)
            .await
            .with_context(|| format!("OpenCode {context}"))?
            .json()
            .await
            .with_context(|| format!("Unexpected OpenCode V2 response for {context}"))
    }

    pub async fn session(&self, id: &str) -> Result<Session> {
        Ok(self
            .json::<Data<Session>>(Method::GET, &format!("api/session/{id}"), None)
            .await?
            .data)
    }

    pub async fn create(&self, id: &str, directory: &std::path::Path) -> Result<Session> {
        Ok(self
            .json::<Data<Session>>(
                Method::POST,
                "api/session",
                Some(json!({
                    "id": id, "location": { "directory": directory },
                })),
            )
            .await?
            .data)
    }

    pub async fn messages(&self, id: &str) -> Result<Vec<Message>> {
        // Retain the newest page, in chronological display order.
        let mut messages = self
            .json::<Data<Vec<Message>>>(
                Method::GET,
                &format!("api/session/{id}/message?limit=200&order=desc"),
                None,
            )
            .await?
            .data;
        messages.reverse();
        Ok(messages)
    }

    pub async fn message(&self, session: &str, message: &str) -> Result<Option<Message>> {
        let response = self
            .request(
                Method::GET,
                &format!("api/session/{session}/message/{message}"),
            )?
            .timeout(Duration::from_secs(20))
            .send()
            .await?;
        // Inbox control items (for example compaction) do not become messages.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(
            Self::checked(response)
                .await?
                .json::<Data<Message>>()
                .await?
                .data,
        ))
    }

    pub async fn inbox(&self, session: &str) -> Result<Vec<InboxEntry>> {
        Ok(self
            .json::<Data<Vec<InboxEntry>>>(
                Method::GET,
                &format!("api/session/{session}/inbox"),
                None,
            )
            .await?
            .data)
    }

    pub async fn permissions(&self, session: &str) -> Result<Vec<PermissionRequest>> {
        Ok(self
            .json::<Data<Vec<PermissionRequest>>>(
                Method::GET,
                &format!("api/session/{session}/permission"),
                None,
            )
            .await?
            .data)
    }

    pub async fn forms(&self, session: &str) -> Result<Vec<Form>> {
        Ok(self
            .json::<Data<Vec<Form>>>(Method::GET, &format!("api/session/{session}/form"), None)
            .await?
            .data)
    }

    async fn decision(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<ReplyStatus> {
        let mut request = self.request(method, path)?.timeout(Duration::from_secs(20));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| {
            anyhow::anyhow!(
            "Request failed; its outcome is uncertain. Refresh pending requests before retrying")
        })?;
        match response.status().as_u16() {
            200..=299 => Ok(ReplyStatus::Applied),
            404 | 409 => Ok(ReplyStatus::Settled),
            400 => {
                let body = response.json::<Value>().await.unwrap_or(Value::Null);
                let message = body
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("The server rejected the answer");
                bail!("{}", message.chars().take(512).collect::<String>())
            }
            _ => {
                Self::checked(response).await?;
                unreachable!()
            }
        }
    }

    pub async fn permission_reply(
        &self,
        session: &str,
        id: &str,
        decision: PermissionDecision,
    ) -> Result<ReplyStatus> {
        self.decision(
            Method::POST,
            &format!("api/session/{session}/permission/{id}/reply"),
            Some(json!({"decision":decision.wire()})),
        )
        .await
    }

    pub async fn form_reply(
        &self,
        session: &str,
        id: &str,
        answer: Option<&serde_json::Map<String, Value>>,
    ) -> Result<ReplyStatus> {
        match answer {
            Some(answer) => {
                self.decision(
                    Method::POST,
                    &format!("api/session/{session}/form/{id}/reply"),
                    Some(json!({"answer":answer})),
                )
                .await
            }
            None => {
                self.decision(
                    Method::DELETE,
                    &format!("api/session/{session}/form/{id}"),
                    None,
                )
                .await
            }
        }
    }

    pub async fn changes(&self, session: &str) -> Result<Vec<FileChange>> {
        Ok(self
            .json::<Data<Vec<FileChange>>>(
                Method::GET,
                &format!("api/session/{session}/diff?context=3"),
                None,
            )
            .await?
            .data)
    }

    pub async fn send(
        &self,
        session: &str,
        id: &str,
        text: &str,
        snippets: &[super::snippets::Snippet],
    ) -> Result<()> {
        self.json::<Value>(
            Method::POST,
            &format!("api/session/{session}/prompt"),
            Some(super::snippets::prompt(id, text, snippets)?),
        )
        .await?;
        Ok(())
    }

    pub async fn interrupt(&self, session: &str) -> Result<()> {
        self.json::<Value>(
            Method::POST,
            &format!("api/session/{session}/interrupt"),
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn events(&self) -> Result<Response> {
        let request = self
            .request(Method::GET, "api/event")?
            .header("accept", "text/event-stream");
        let response = tokio::time::timeout(Duration::from_secs(20), request.send())
            .await
            .context("Timed out opening OpenCode events")?
            .map_err(|_| anyhow::anyhow!("Cannot open OpenCode event stream"))?;
        let response = Self::checked(response).await?;
        if !response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"))
        {
            bail!("OpenCode did not return an event stream");
        }
        Ok(response)
    }

    #[cfg(test)]
    pub(super) async fn remove_test_session(&self, id: &str) -> Result<()> {
        let response = self
            .request(Method::DELETE, &format!("api/session/{id}"))?
            .timeout(Duration::from_secs(20))
            .send()
            .await?;
        Self::checked(response).await?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct Event {
    #[serde(rename = "type")]
    pub kind: String,
    pub data: Value,
}

/// Handles split UTF-8, CRLF, comments, and multiline SSE data.
#[derive(Default)]
pub(super) struct EventDecoder {
    buffer: Vec<u8>,
    data: Vec<u8>,
}

impl EventDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Event>> {
        const MAX_FRAME: usize = 16 * 1024 * 1024;
        self.buffer.extend_from_slice(chunk);
        if self.buffer.len() + self.data.len() > MAX_FRAME {
            bail!("OpenCode event exceeds the size limit");
        }
        let mut events = Vec::new();
        let mut consumed = 0;
        while let Some(end) = self.buffer[consumed..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let line = &self.buffer[consumed..consumed + end];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            consumed += end + 1;
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(
                        serde_json::from_slice(&self.data).context("Invalid OpenCode event")?,
                    );
                    self.data.clear();
                }
            } else if let Some(data) = line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(data);
            }
        }
        self.buffer.drain(..consumed);
        Ok(events)
    }
}
