use super::*;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn event(kind: &str, data: Value) -> Event {
    Event {
        kind: kind.into(),
        data,
    }
}

#[test]
fn sse_handles_chunked_utf8_comments_and_multiline_crlf() -> Result<()> {
    let input = ": heartbeat\r\nevent: message\r\ndata: {\"type\":\"session.text.delta\",\r\ndata: \"data\":{\"delta\":\"héllo\"}}\r\n\r\n";
    let mut decoder = EventDecoder::default();
    let mut events = Vec::new();
    for byte in input.as_bytes() {
        events.extend(decoder.push(&[*byte])?);
    }
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["delta"], "héllo");
    assert!(decoder.push(b"data: invalid\n\n").is_err());
    Ok(())
}

#[test]
fn text_streams_are_per_kind_and_completed_values_are_authoritative() -> Result<()> {
    let mut state = State::default();
    state.apply(event("session.step.started", json!({"assistantMessageID":"msg_a", "agent":"build", "model":{"providerID":"test","id":"chat"}})))?;
    state.apply(event(
        "session.reasoning.started",
        json!({"assistantMessageID":"msg_a","ordinal":0}),
    ))?;
    state.apply(event(
        "session.text.delta",
        json!({"assistantMessageID":"msg_a","ordinal":0,"delta":"hel"}),
    ))?;
    state.apply(event(
        "session.text.delta",
        json!({"assistantMessageID":"msg_a","ordinal":0,"delta":"lo"}),
    ))?;
    assert_eq!(state.messages[0].content[1].text, "hello");
    state.apply(event(
        "session.text.ended",
        json!({"assistantMessageID":"msg_a","ordinal":0,"text":"complete"}),
    ))?;
    state.apply(event(
        "session.text.delta",
        json!({"assistantMessageID":"msg_a","ordinal":0,"delta":"old queued delta"}),
    ))?;
    assert_eq!(state.messages[0].content[1].text, "complete");
    state.apply(event(
        "permission.asked",
        json!({"id":"per_test","action":"shell"}),
    ))?;
    assert_eq!(state.notices.len(), 1);
    state.apply(event("permission.replied", json!({"requestID":"per_test"})))?;
    assert!(state.notices.is_empty());
    state.apply(event(
        "form.created",
        json!({"form":{"id":"form_test","sessionID":"ses_test"}}),
    ))?;
    assert_eq!(state.notices.len(), 1);
    state.apply(event(
        "form.cancelled",
        json!({"id":"form_test","sessionID":"ses_test"}),
    ))?;
    assert!(state.notices.is_empty());
    state.apply(event(
        "session.execution.failed",
        json!({"error":{"message":"provider unavailable"}}),
    ))?;
    assert_eq!(state.error.as_deref(), Some("provider unavailable"));
    assert!(!state.busy);
    Ok(())
}

#[test]
fn endpoint_validation_and_ids_do_not_allow_credentials_or_path_injection() {
    for url in [
        "file:///secret",
        "http://user:secret@localhost:4096",
        "http://localhost:4096/api",
        "http://localhost:4096/?secret=1",
    ] {
        assert!(Endpoint::new(url, None, None).is_err());
    }
    assert!(Endpoint::new("http://localhost:4096", None, None).is_ok());
    assert!(valid_id("ses_existing", "ses"));
    assert!(!valid_id("ses_a/../other", "ses"));
    assert_ne!(new_id("msg"), new_id("msg"));
}

#[test]
fn unrelated_events_with_conflicting_fields_do_not_disconnect_chat() -> Result<()> {
    let mut state = State::default();
    state.apply(event(
        "session.instructions.updated",
        json!({
            "sessionID": "ses_test", "agent": {"build": []}, "model": {}, "id": {}
        }),
    ))?;
    state.apply(event(
        "session.future.event",
        json!({"ordinal": "not a number"}),
    ))?;
    assert!(state.messages.is_empty());
    Ok(())
}

#[test]
fn tool_completion_keeps_input_and_exposes_output_for_details() -> Result<()> {
    let mut state = State::default();
    state.apply(event(
        "session.tool.called",
        json!({
            "assistantMessageID": "msg_a", "id": "call_read", "name": "read",
            "input": {"path": "src/main.rs"}
        }),
    ))?;
    state.apply(event(
        "session.tool.success",
        json!({
            "assistantMessageID": "msg_a", "id": "call_read",
            "content": [{"type":"text", "text":"file contents"}]
        }),
    ))?;
    let part = &state.messages[0].content[0];
    let tool = part.state.as_ref().unwrap();
    assert_eq!(part.name, "read");
    assert_eq!(tool.status, "completed");
    assert_eq!(tool.input.as_ref().unwrap()["path"], "src/main.rs");
    assert_eq!(tool.content[0]["text"], "file contents");
    Ok(())
}

struct Mock {
    url: String,
    messages: Arc<Mutex<Vec<Value>>>,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    events: mpsc::Sender<String>,
    permissions: Arc<Mutex<Vec<Value>>>,
    forms: Arc<Mutex<Vec<Value>>>,
    reply_status: Arc<Mutex<Option<u16>>>,
    changes: Arc<Mutex<Vec<Value>>>,
    task: JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Mock {
    async fn new(auth: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let messages = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let permissions = Arc::new(Mutex::new(Vec::<Value>::new()));
        let forms = Arc::new(Mutex::new(Vec::<Value>::new()));
        let reply_status = Arc::new(Mutex::new(None::<u16>));
        let changes = Arc::new(Mutex::new(Vec::<Value>::new()));
        let (events, rx) = mpsc::channel::<String>(32);
        let receiver = Arc::new(Mutex::new(Some(rx)));
        let content = messages.clone();
        let recorded = requests.clone();
        let pending_permissions = permissions.clone();
        let pending_forms = forms.clone();
        let status = reply_status.clone();
        let diffs = changes.clone();
        let selected_model = Arc::new(Mutex::new(json!({"providerID":"test","id":"chat"})));
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let content = content.clone();
                let recorded = recorded.clone();
                let receiver = receiver.clone();
                let pending_permissions = pending_permissions.clone();
                let pending_forms = pending_forms.clone();
                let status = status.clone();
                let diffs = diffs.clone();
                let selected_model = selected_model.clone();
                tokio::spawn(async move {
                    let mut input = Vec::new();
                    let header_end = loop {
                        let mut buf = [0; 4096];
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        input.extend_from_slice(&buf[..n]);
                        if let Some(index) =
                            input.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            break index + 4;
                        }
                    };
                    let headers = String::from_utf8(input[..header_end].to_vec()).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    while input.len() < header_end + length {
                        let mut buf = [0; 4096];
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        input.extend_from_slice(&buf[..n]);
                    }
                    let request = headers.lines().next().unwrap().to_owned();
                    let path = request.split_whitespace().nth(1).unwrap();
                    let body = if length == 0 {
                        Value::Null
                    } else {
                        serde_json::from_slice(&input[header_end..header_end + length]).unwrap()
                    };
                    recorded.lock().push((request.clone(), body.clone()));
                    let authenticated = headers
                        .to_ascii_lowercase()
                        .contains("authorization: basic b3blbmnvzgu6dgvzda==");
                    if auth && !authenticated {
                        socket.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
                        return;
                    }
                    if path == "/api/event" {
                        let rx = receiver.lock().take();
                        let Some(mut rx) = rx else {
                            return;
                        };
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"server.connected\",\"data\":{}}\n\n").await.unwrap();
                        while let Some(event) = rx.recv().await {
                            if socket
                                .write_all(format!("data: {event}\n\n").as_bytes())
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        return;
                    }
                    if (path.contains("/permission/") && path.ends_with("/reply"))
                        || (path.contains("/form/")
                            && (path.ends_with("/reply") || request.starts_with("DELETE ")))
                    {
                        let code = status.lock().take().unwrap_or(204);
                        if matches!(code, 204 | 404 | 409) {
                            let segments: Vec<_> = path.split('/').collect();
                            let id = segments[5];
                            if path.contains("/permission/") {
                                pending_permissions.lock().retain(|p| p["id"] != id);
                            } else {
                                pending_forms.lock().retain(|f| f["id"] != id);
                            }
                        }
                        let body = if code == 204 {
                            String::new()
                        } else {
                            json!({"message":"Invalid answer or stale request"}).to_string()
                        };
                        socket.write_all(format!("HTTP/1.1 {code} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                        return;
                    }
                    if path == "/api/session/ses_test/model" {
                        let code = status.lock().take().unwrap_or(204);
                        if code == 204 {
                            *selected_model.lock() = body["model"].clone();
                        }
                        socket.write_all(format!("HTTP/1.1 {code} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                        return;
                    }
                    let session = json!({"id":"ses_test","location":{"directory":"/work space"},"time":{},"agent":"build","model":*selected_model.lock()});
                    let response = if path == "/api/info" {
                        json!({"pid":42,"version":"2.1.0"})
                    } else if path.starts_with("/api/model?") {
                        json!({"location":{"directory":"/work space"},"data":[
                            {"id":"chat","providerID":"test","name":"Test chat","enabled":true,"status":"active","variants":[{"id":"none"},{"id":"high"}]}
                        ]})
                    } else if path == "/api/session" || path == "/api/session/ses_test" {
                        json!({"data":session})
                    } else if path.ends_with("/permission") {
                        json!({"data": *pending_permissions.lock()})
                    } else if path.ends_with("/form") {
                        json!({"data": *pending_forms.lock()})
                    } else if path.contains("/diff?") {
                        json!({"data": *diffs.lock()})
                    } else if path.ends_with("/inbox") {
                        json!({"data":[]})
                    } else if path.ends_with("/prompt") {
                        content.lock().push(
                             json!({"id":body["id"],"type":"user","text":body["text"],"files":body.get("files").cloned().unwrap_or_else(|| json!([])),"metadata":body["metadata"],"time":{}}),
                        );
                        json!({"data":{"id":body["id"],"sessionID":"ses_test","type":"user","payload":{"text":body["text"]}}})
                    } else if path.ends_with("/interrupt") {
                        json!({"interrupted":true})
                    } else if path.contains("/message?") {
                        json!({"data":content.lock().iter().rev().cloned().collect::<Vec<_>>(),"cursor":{}})
                    } else if path.contains("/message/") {
                        let id = path.rsplit('/').next().unwrap();
                        json!({"data":content.lock().iter().find(|message| message["id"] == id).unwrap().clone()})
                    } else {
                        panic!("unexpected mock request: {request}");
                    };
                    let body = response.to_string();
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                });
            }
        });
        Self {
            url,
            messages,
            requests,
            events,
            task,
            permissions,
            forms,
            reply_status,
            changes,
        }
    }

    async fn event(&self, kind: &str, data: Value) {
        self.events
            .send(json!({"type":kind,"data":data}).to_string())
            .await
            .unwrap();
    }
}

async fn wait(shared: &Mutex<State>, ready: impl Fn(&State) -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if ready(&shared.lock()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("OpenCode mock operation did not complete");
}

async fn mock_connection(mock: &Mock) -> Result<Connection> {
    let api = Api::connect(Endpoint::new(&mock.url, None, None)?).await?;
    let state = Arc::new(Mutex::new(State::default()));
    let (requests, rx) = mpsc::channel(8);
    let shared = state.clone();
    let task = tokio::spawn(async move {
        if let Err(error) = run_with_api(
            api,
            "/work space".into(),
            "ses_test",
            true,
            shared.clone(),
            rx,
        )
        .await
        {
            shared.lock().error = Some(format!("{error:#}"));
        }
    });
    wait(&state, |state| state.connected).await;
    Ok(Connection {
        state,
        id: "ses_test".into(),
        directory: "/work space".into(),
        attach: true,
        requests,
        task,
    })
}

#[tokio::test]
async fn selected_snippets_use_v2_files_and_desktop_metadata_and_survive_history_reload(
) -> Result<()> {
    let mock = Mock::new(false).await;
    let mut connection = mock_connection(&mock).await?;
    let snippet = snippets::Snippet {
        comment: "Explain this selected expression".into(),
        path: Some("/work space/source.rs".into()),
        text: "unsaved é + snippet\n".into(),
        selection: snippets::Selection {
            start_line: 3,
            end_line: 3,
            start_char: 0,
            end_char: 19,
        },
    };
    connection.send_snippets("Explain this".into(), vec![snippet.clone()])?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while mock.messages.lock().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let posted = mock
        .requests
        .lock()
        .iter()
        .find(|(request, _)| request.contains("/prompt "))
        .unwrap()
        .1
        .clone();
    assert!(posted["text"].as_str().unwrap().contains("The user made the following comment regarding line 3 of /work space/source.rs: Explain this selected expression"));
    assert_eq!(posted["metadata"]["displayText"], "Explain this");
    assert_eq!(posted["metadata"]["comments"][0]["preview"], snippet.text);
    assert_eq!(
        posted["metadata"]["comments"][0]["comment"],
        snippet.comment
    );
    assert_eq!(
        posted["files"][0]["source"]["uri"],
        "file:///work%20space/source.rs?start=3&end=3"
    );
    assert!(posted["files"][0]["uri"]
        .as_str()
        .unwrap()
        .starts_with("data:text/plain;"));
    let api = Api::connect(Endpoint::new(&mock.url, None, None)?).await?;
    snapshot(&api, "ses_test", &connection.state).await?;
    assert_eq!(connection.state.lock().messages[0].files.len(), 1);
    assert_eq!(
        connection.state.lock().messages[0]
            .metadata
            .as_ref()
            .unwrap()["comments"][0]["comment"],
        snippet.comment
    );
    assert!(connection.state.lock().pending.is_empty());
    Ok(())
}

#[tokio::test]
async fn reference_comment_can_be_sent_without_an_overall_message() -> Result<()> {
    let mock = Mock::new(false).await;
    let mut connection = mock_connection(&mock).await?;
    assert!(connection.send("".into()).is_err());
    let snippet = snippets::Snippet {
        path: Some("/work space/source.rs".into()),
        text: "selected code".into(),
        selection: snippets::Selection {
            start_line: 2,
            end_line: 2,
            start_char: 0,
            end_char: 13,
        },
        comment: "Why does this fail?".into(),
    };
    connection.send_snippets("".into(), vec![snippet])?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while mock.messages.lock().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let posted = mock
        .requests
        .lock()
        .iter()
        .find(|(request, _)| request.contains("/prompt "))
        .unwrap()
        .1
        .clone();
    assert_eq!(posted["metadata"]["displayText"], "");
    assert_eq!(
        posted["metadata"]["comments"][0]["comment"],
        "Why does this fail?"
    );
    assert!(posted["text"]
        .as_str()
        .unwrap()
        .contains("Why does this fail?"));
    Ok(())
}

#[tokio::test]
async fn model_catalog_is_session_scoped_and_switch_handles_empty_204() -> Result<()> {
    let mock = Mock::new(false).await;
    let connection = mock_connection(&mock).await?;
    let handle = connection.model_handle()?;
    let rows = models::choices(handle.catalog()?.await??, handle.current().as_ref());
    assert_eq!(rows.len(), 3);
    assert!(rows[0].current);
    assert!(rows[0].model.variant.is_none());
    assert!(mock
        .requests
        .lock()
        .iter()
        .any(|(request, _)| request
            == "GET /api/model?location%5Bdirectory%5D=%2Fwork+space HTTP/1.1"));
    // Catalog inspection and cancellation do not mutate the session.
    assert!(mock
        .requests
        .lock()
        .iter()
        .all(|(request, _)| request.starts_with("GET ")));
    let mut selected = rows
        .iter()
        .find(|row| row.model.variant.as_deref() == Some("none"))
        .unwrap()
        .model
        .clone();
    handle.switch(selected.clone())?.await??;
    assert_eq!(handle.current().as_ref(), Some(&selected));
    let posted = mock
        .requests
        .lock()
        .iter()
        .find(|(request, _)| request.starts_with("POST /api/session/ses_test/model "))
        .unwrap()
        .1
        .clone();
    assert_eq!(
        posted,
        json!({"model":{"providerID":"test","id":"chat","variant":"none"}})
    );
    selected.variant = None;
    handle.switch(selected.clone())?.await??;
    assert_eq!(handle.current().as_ref(), Some(&selected));
    let posted = mock
        .requests
        .lock()
        .iter()
        .rev()
        .find(|(request, _)| request.starts_with("POST /api/session/ses_test/model "))
        .unwrap()
        .1
        .clone();
    assert!(posted["model"].get("variant").is_none());
    Ok(())
}

#[tokio::test]
async fn rejected_model_changes_preserve_selection_and_connection() -> Result<()> {
    let mock = Mock::new(false).await;
    let connection = mock_connection(&mock).await?;
    let handle = connection.model_handle()?;
    let original = handle.current();
    let mut changed = original.clone().unwrap();
    changed.variant = Some("high".into());
    *mock.reply_status.lock() = Some(400);
    assert!(handle.switch(changed)?.await?.is_err());
    assert_eq!(handle.current(), original);
    assert!(connection.state.lock().connected);
    Ok(())
}

#[tokio::test]
async fn model_handles_reject_replaced_closed_and_reconnected_sessions() -> Result<()> {
    let mock = Mock::new(false).await;
    let mut connection = mock_connection(&mock).await?;
    let handle = connection.model_handle()?;
    assert!(connection.matches_model_handle(&handle));
    let other = Connection::for_test();
    assert!(!other.matches_model_handle(&handle));
    connection.reconnect();
    assert!(!connection.matches_model_handle(&handle));
    // Wait for the aborted worker's receiver to close without using the real service.
    connection.task.abort();
    let _ = (&mut connection.task).await;
    tokio::task::yield_now().await;
    assert!(handle.catalog().is_err());
    drop(connection);
    assert!(handle.switch(handle.current().unwrap()).is_err());
    Ok(())
}

fn permission() -> Value {
    json!({"id":"per_test", "sessionID":"ses_test", "action":"edit", "resources":["/work space/example.txt"],
        "metadata":{"command":"write example"}, "save":["/work space/*"]})
}

fn form() -> Value {
    json!({"id":"frm_test", "sessionID":"ses_test", "title":"Choose test settings", "fields":[
        {"key":"confirm","type":"boolean","required":true},
        {"key":"count","type":"integer","minimum":1,"maximum":3,"default":2},
        {"key":"items","type":"multiselect","options":[{"value":"a","label":"A"}],"default":["a"]}
    ]})
}

#[tokio::test]
async fn permissions_require_explicit_decisions_and_submit_only_once() -> Result<()> {
    use super::interactions::PermissionDecision;
    let mock = Mock::new(false).await;
    mock.permissions.lock().push(permission());
    let connection = mock_connection(&mock).await?;
    assert!(mock
        .requests
        .lock()
        .iter()
        .all(|(request, _)| request.starts_with("GET ")));
    let expected = connection.state.lock().permissions["per_test"].clone();
    let handle = connection.interactions();
    handle.permission(expected.clone(), PermissionDecision::Once)?;
    assert!(handle
        .permission(expected, PermissionDecision::Once)
        .is_err());
    wait(&connection.state, |state| {
        state.permissions.is_empty() && state.resolving.is_empty()
    })
    .await;
    assert!(connection.state.lock().connected);
    let replies: Vec<_> = mock
        .requests
        .lock()
        .iter()
        .filter(|(request, _)| request.contains("/permission/per_test/reply"))
        .cloned()
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].1, json!({"decision":"once"}));
    Ok(())
}

#[tokio::test]
async fn stale_or_changed_permissions_never_replay_approval() -> Result<()> {
    use super::interactions::PermissionDecision;
    for race in [None, Some(404), Some(409)] {
        let mock = Mock::new(false).await;
        mock.permissions.lock().push(permission());
        let connection = mock_connection(&mock).await?;
        let expected = connection.state.lock().permissions["per_test"].clone();
        if let Some(status) = race {
            *mock.reply_status.lock() = Some(status);
        } else {
            mock.permissions.lock().clear();
        }
        connection
            .interactions()
            .permission(expected, PermissionDecision::Once)?;
        wait(&connection.state, |state| {
            state.permissions.is_empty() && state.resolving.is_empty()
        })
        .await;
        assert!(connection.state.lock().connected);
        assert!(connection
            .state
            .lock()
            .interaction_status
            .as_ref()
            .unwrap()
            .contains("already settled"));
        let count = mock
            .requests
            .lock()
            .iter()
            .filter(|(request, _)| request.contains("/permission/per_test/reply"))
            .count();
        assert_eq!(count, usize::from(race.is_some()));
    }
    let mock = Mock::new(false).await;
    mock.permissions.lock().push(permission());
    let connection = mock_connection(&mock).await?;
    let expected = connection.state.lock().permissions["per_test"].clone();
    mock.permissions.lock()[0]["resources"] = json!(["/different/scope"]);
    connection
        .interactions()
        .permission(expected, PermissionDecision::Once)?;
    wait(&connection.state, |state| state.resolving.is_empty()).await;
    assert!(connection
        .state
        .lock()
        .interaction_status
        .as_ref()
        .unwrap()
        .contains("details changed"));
    assert!(!mock
        .requests
        .lock()
        .iter()
        .any(|(request, _)| request.starts_with("POST ")));
    Ok(())
}

#[tokio::test]
async fn rejected_form_answers_remain_editable_and_conflicts_are_reconciled() -> Result<()> {
    let mock = Mock::new(false).await;
    mock.forms.lock().push(form());
    let connection = mock_connection(&mock).await?;
    let expected = connection.state.lock().forms["frm_test"].clone();
    let answer = json!({"confirm":true, "count":2, "items":["a"]})
        .as_object()
        .unwrap()
        .clone();
    *mock.reply_status.lock() = Some(400);
    connection
        .interactions()
        .form(expected.clone(), Some(answer.clone()))?;
    wait(&connection.state, |state| state.resolving.is_empty()).await;
    assert!(connection.state.lock().forms.contains_key("frm_test"));
    assert!(connection.state.lock().connected);
    assert!(connection
        .state
        .lock()
        .interaction_status
        .as_ref()
        .unwrap()
        .contains("Invalid answer"));
    *mock.reply_status.lock() = Some(409);
    connection
        .interactions()
        .form(expected, Some(answer.clone()))?;
    wait(&connection.state, |state| {
        state.forms.is_empty() && state.resolving.is_empty()
    })
    .await;
    let replies: Vec<_> = mock
        .requests
        .lock()
        .iter()
        .filter(|(request, _)| request.contains("/form/frm_test/reply"))
        .cloned()
        .collect();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0].1, json!({"answer":answer}));
    Ok(())
}

#[tokio::test]
async fn reviewing_changes_is_read_only_and_preserves_structured_patches() -> Result<()> {
    let mock = Mock::new(false).await;
    mock.changes.lock().push(json!({
        "file":"example.rs", "patch":"@@ -0,0 +1 @@\n+hello\n", "additions":1, "deletions":0, "status":"added"
    }));
    let connection = mock_connection(&mock).await?;
    connection.interactions().review()?;
    wait(&connection.state, |state| {
        !state.review_loading && state.review.is_some()
    })
    .await;
    let state = connection.state.lock();
    let changes = state.review.as_ref().unwrap();
    assert_eq!(changes[0].file, "example.rs");
    assert!(changes[0].patch.contains("+hello"));
    assert!(mock
        .requests
        .lock()
        .iter()
        .all(|(request, _)| request.starts_with("GET ")));
    Ok(())
}

#[tokio::test]
async fn rejection_and_form_cancellation_do_not_change_permission_rules() -> Result<()> {
    let mock = Mock::new(false).await;
    mock.permissions.lock().push(permission());
    mock.forms.lock().push(form());
    let connection = mock_connection(&mock).await?;
    let permission = connection.state.lock().permissions["per_test"].clone();
    let form = connection.state.lock().forms["frm_test"].clone();
    connection
        .interactions()
        .permission(permission, super::interactions::PermissionDecision::Reject)?;
    connection.interactions().form(form, None)?;
    wait(&connection.state, |state| {
        state.resolving.is_empty() && state.permissions.is_empty() && state.forms.is_empty()
    })
    .await;
    let requests = mock.requests.lock();
    assert!(requests.iter().any(|(request, body)| request
        .starts_with("POST /api/session/ses_test/permission/per_test/reply ")
        && *body == json!({"decision":"reject"})));
    assert!(requests
        .iter()
        .any(|(request, _)| request.starts_with("DELETE /api/session/ses_test/form/frm_test ")));
    assert!(!requests
        .iter()
        .any(|(request, _)| request.starts_with("PATCH ") || request.starts_with("PUT ")));
    Ok(())
}

#[test]
fn forms_validate_types_defaults_and_cascading_visibility() -> Result<()> {
    use super::interactions::Form;
    let form: Form = serde_json::from_value(
        json!({"id":"frm_test", "sessionID":"ses_test", "title":"Test", "fields":[
            {"key":"enabled","type":"boolean","default":false},
            {"key":"choice","type":"string","options":[{"value":"a","label":"A"}],"required":true,"when":[{"key":"enabled","op":"eq","value":true}]},
            {"key":"count","type":"integer","minimum":1,"maximum":3,"when":[{"key":"choice","op":"neq","value":"b"}]},
            {"key":"hidden","type":"string","hidden":true,"default":"server default"}
        ]}),
    )?;
    let raw = json!({"enabled":false,"choice":"a","count":2})
        .as_object()
        .unwrap()
        .clone();
    let answer = form.normalize(&raw);
    assert!(!answer.contains_key("choice"));
    assert!(!answer.contains_key("count"));
    assert_eq!(answer["hidden"], "server default");
    form.validate(&answer)?;
    assert!(form.fields[1].parse("not an option").is_err());
    assert!(form.fields[2].parse("1.5").is_err());
    assert!(form.fields[2].parse("4").is_err());
    assert!(form.fields[2].parse("2").is_ok());
    let form: Form = serde_json::from_value(
        json!({"id":"frm_numeric","title":"Numeric conditions","fields":[
            {"key":"number","type":"number","default":1.0},
            {"key":"detail","type":"string","default":"visible","when":[{"key":"number","op":"eq","value":1}]},
            {"key":"external","type":"external","url":"https://example.com"}
        ]}),
    )?;
    let mut answer = form.normalize(&serde_json::Map::new());
    assert_eq!(answer["detail"], "visible");
    assert!(
        !answer.contains_key("external"),
        "External steps must never be acknowledged automatically"
    );
    assert!(form.validate(&answer).is_err());
    answer.insert("external".into(), json!(true));
    form.validate(&answer)?;
    Ok(())
}

#[tokio::test]
async fn authenticated_chat_streams_and_interrupts_without_using_a_live_service() -> Result<()> {
    let mock = Mock::new(true).await;
    let endpoint = Endpoint::new(
        &mock.url,
        Some("test".into()),
        Some((42, Some("2.1.0".into()))),
    )?;
    let api = Api::connect(endpoint).await?;
    let state = Arc::new(Mutex::new(State::default()));
    let (tx, rx) = mpsc::channel(8);
    let shared = state.clone();
    let task = tokio::spawn(async move {
        run_with_api(api, "/work space".into(), "ses_test", false, shared, rx).await
    });
    wait(&state, |state| state.connected).await;
    tx.send(Request::Send {
        id: "msg_user".into(),
        text: "hello".into(),
        snippets: Vec::new(),
    })
    .await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while mock.messages.lock().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    mock.event(
        "session.inbox.delivered",
        json!({"sessionID":"ses_test","inboxID":"msg_user"}),
    )
    .await;
    mock.event("session.execution.started", json!({"sessionID":"ses_test"}))
        .await;
    mock.event(
        "session.instructions.updated",
        json!({
            "sessionID": "ses_test", "agent": {"build": []}
        }),
    )
    .await;
    mock.event("session.text.delta", json!({"sessionID":"ses_other","assistantMessageID":"msg_other","ordinal":0,"delta":"wrong session"})).await;
    mock.event("session.text.delta", json!({"sessionID":"ses_test","assistantMessageID":"msg_a","ordinal":0,"delta":"streaming"})).await;
    wait(&state, |state| {
        state
            .messages
            .iter()
            .any(|message| message.content.iter().any(|part| part.text == "streaming"))
    })
    .await;
    assert_eq!(state.lock().messages.len(), 2);
    mock.messages.lock().push(json!({"id":"msg_a","type":"assistant","content":[{"type":"text","text":"final answer"}],"time":{"completed":1}}));
    mock.event("session.text.ended", json!({"sessionID":"ses_test","assistantMessageID":"msg_a","ordinal":0,"text":"final answer"})).await;
    mock.event(
        "session.execution.succeeded",
        json!({"sessionID":"ses_test"}),
    )
    .await;
    wait(&state, |state| {
        !state.busy
            && state.messages.last().is_some_and(|message| {
                message
                    .content
                    .first()
                    .is_some_and(|part| part.text == "final answer")
            })
    })
    .await;
    tx.send(Request::Interrupt).await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !mock
            .requests
            .lock()
            .iter()
            .any(|(request, _)| request.contains("/interrupt"))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    let requests = mock.requests.lock();
    let create = requests
        .iter()
        .find(|(request, _)| request.starts_with("POST /api/session "))
        .unwrap();
    assert_eq!(create.1["location"]["directory"], "/work space");
    let prompt = requests
        .iter()
        .find(|(request, _)| request.contains("/prompt"))
        .unwrap();
    assert_eq!(prompt.1, json!({"id":"msg_user","text":"hello"}));
    task.abort();
    Ok(())
}

#[tokio::test]
async fn authentication_and_stale_identity_errors_are_explicit() -> Result<()> {
    let mock = Mock::new(true).await;
    let error = Api::connect(Endpoint::new(&mock.url, None, None)?)
        .await
        .err()
        .unwrap();
    let error = format!("{error:#}");
    assert!(error.contains("GET api/info"));
    assert!(error.contains("authentication"));
    let error = Api::connect(Endpoint::new(
        &mock.url,
        Some("test".into()),
        Some((999, None)),
    )?)
    .await
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("stale"));
    Ok(())
}

#[tokio::test]
async fn attaching_loads_history_without_creating_a_session_or_sending_a_prompt() -> Result<()> {
    let mock = Mock::new(false).await;
    mock.messages
        .lock()
        .push(json!({"id":"msg_old","type":"user","text":"previous conversation","time":{}}));
    let api = Api::connect(Endpoint::new(&mock.url, None, None)?).await?;
    let state = Arc::new(Mutex::new(State::default()));
    let (_tx, rx) = mpsc::channel(8);
    let shared = state.clone();
    let task = tokio::spawn(async move {
        run_with_api(api, "/unused".into(), "ses_test", true, shared, rx).await
    });
    wait(&state, |state| state.connected).await;
    assert_eq!(state.lock().messages[0].text, "previous conversation");
    assert!(mock
        .requests
        .lock()
        .iter()
        .all(|(request, _)| request.starts_with("GET ")));
    task.abort();
    Ok(())
}
