//! Paginated, read-only browsing without creating a chat or starting a service.
use super::*;

#[derive(Deserialize)]
pub(super) struct SessionPage {
    pub data: Vec<Session>,
    pub cursor: SessionCursor,
}

#[derive(Deserialize)]
pub(super) struct SessionCursor {
    pub next: Option<String>,
}

#[derive(Clone)]
pub(crate) struct SessionBrowser {
    api: Arc<Api>,
}

impl SessionBrowser {
    pub async fn connect() -> Result<Self> {
        Ok(Self::new(Api::connect(Endpoint::discover().await?).await?))
    }

    pub(super) fn new(api: Api) -> Self {
        Self { api: Arc::new(api) }
    }

    pub async fn session(&self, id: &str) -> Result<Session> {
        anyhow::ensure!(valid_id(id, "ses"), "Invalid OpenCode session ID");
        let session = self.api.session(id).await?;
        anyhow::ensure!(session.id == id, "OpenCode returned a different session");
        anyhow::ensure!(
            session.location.directory.is_absolute(),
            "OpenCode session directory is not absolute"
        );
        Ok(session)
    }

    /// Fetch more pages only while the picker remains open. Cursor tokens are
    /// opaque and can contain URL-special characters; the transport encodes them.
    pub async fn stream(
        &self,
        mut push: impl FnMut(Session) -> bool,
        cancelled: impl Fn() -> bool,
    ) -> Result<usize> {
        let mut cursor = None;
        let mut cursors = HashSet::new();
        let mut sessions = HashSet::new();
        let mut count = 0;
        for _ in 0..100 {
            if cancelled() {
                return Ok(count);
            }
            let page = self.api.sessions_page(cursor.as_deref()).await?;
            if cancelled() {
                return Ok(count);
            }
            for session in page.data {
                anyhow::ensure!(
                    valid_id(&session.id, "ses"),
                    "OpenCode returned an invalid session ID"
                );
                anyhow::ensure!(
                    session.location.directory.is_absolute(),
                    "OpenCode session directory is not absolute"
                );
                if !sessions.insert(session.id.clone()) {
                    continue;
                }
                anyhow::ensure!(
                    count < 10_000,
                    "Session picker reached its 10,000-session limit"
                );
                if !push(session) {
                    return Ok(count);
                }
                count += 1;
            }
            let Some(next) = page.cursor.next else {
                return Ok(count);
            };
            anyhow::ensure!(
                cursors.insert(next.clone()),
                "OpenCode repeated a session-list cursor"
            );
            cursor = Some(next);
        }
        anyhow::bail!("Session picker reached its 10,000-session limit")
    }
}
