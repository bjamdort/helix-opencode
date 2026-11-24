//! Explicitly opt-in: these tests use the registered service and model inference.
use super::*;
use client::Data;
use reqwest::Method;
use serde_json::json;

#[tokio::test]
#[ignore = "lists and attaches an isolated existing V2 session; no model inference"]
async fn live_service_session_picker_listing_and_attach() -> Result<()> {
    let api = Api::connect(Endpoint::discover().await?).await?;
    let directory = tempfile::Builder::new()
        .prefix("helix-session-picker-")
        .tempdir_in("/tmp/opencode")?;
    let id = new_id("ses");
    api.create(&id, directory.path()).await?;
    let result: Result<()> = async {
        let browser = sessions::SessionBrowser::connect().await?;
        let mut listed = None;
        browser
            .stream(
                |session| {
                    if session.id == id {
                        listed = Some(session);
                        false
                    } else {
                        true
                    }
                },
                || false,
            )
            .await?;
        anyhow::ensure!(
            listed.is_some(),
            "Session picker did not list the fixture session"
        );
        let selected = browser.session(&id).await?;
        anyhow::ensure!(
            selected.location.directory == directory.path(),
            "Picker did not retain the selected session directory"
        );
        let connection = Connection::new(selected.location.directory, Some(selected.id));
        wait_for(&connection, |state| state.connected).await?;
        anyhow::ensure!(
            connection
                .state
                .lock()
                .session
                .as_ref()
                .is_some_and(|session| session.id == id),
            "Picker did not attach the selected existing session"
        );
        anyhow::ensure!(
            api.messages(&id).await?.is_empty(),
            "Browsing or attaching unexpectedly sent a prompt"
        );
        Ok(())
    }
    .await;
    let removed = api.remove_test_session(&id).await;
    result?;
    removed?;
    Ok(())
}

#[tokio::test]
#[ignore = "uses explicit model inference for two scoped fixture edits and verifies historical turn snapshots"]
async fn live_service_historical_turn_changes() -> Result<()> {
    use interactions::PermissionDecision;
    let api = Api::connect(Endpoint::discover().await?).await?;
    let directory = tempfile::Builder::new()
        .prefix("helix-turn-review-")
        .tempdir_in("/tmp/opencode")?;
    let marker = directory.path().join("marker.txt");
    std::fs::write(&marker, "BASE\n")?;
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "marker.txt"],
        vec!["commit", "--quiet", "-m", "initial"],
    ] {
        let output = std::process::Command::new("git")
            .current_dir(directory.path())
            .args([
                "-c",
                "user.name=Helix test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "core.hooksPath=",
                "-c",
                "commit.gpgSign=false",
            ])
            .args(args)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "Cannot prepare the isolated turn-review repository"
        );
    }
    let id = fixture_with_rules(
        &api,
        directory.path(),
        json!([
            {"action":"*","resource":"*","effect":"deny"},
            {"action":"edit","resource":"marker.txt","effect":"ask"},
            {"action":"read","resource":"marker.txt","effect":"allow"}
        ]),
    )
    .await?;
    let result: Result<()> = async {
        let mut connection = Connection::new(directory.path().to_owned(), Some(id.clone()));
        wait_for(&connection, |state| state.connected).await?;
        let mut anchors = Vec::new();
        for (contents, token) in [("TURN_ONE","HELIX_TURN_ONE_DONE"),("TURN_TWO","HELIX_TURN_TWO_DONE")] {
            connection.send(format!("Isolated integration test: use write or edit to make marker.txt contain exactly {contents} followed by one newline. Do not modify any other file or use shell. Then reply exactly {token}."))?;
            anchors.push(connection.state.lock().pending.last().context("Missing local prompt")?.id.clone());
            wait_for(&connection, |state| !state.permissions.is_empty() || !state.busy && has_reply(state,token)).await?;
            let permission = connection.state.lock().permissions.values().next().cloned().context("Agent did not request fixture edit approval")?;
            anyhow::ensure!(permission.action == "edit" && permission.resources == ["marker.txt"],
                "Refusing an approval outside the fixture file");
            connection.interactions().permission(permission, PermissionDecision::Once)?;
            wait_for(&connection, |state| !state.busy && has_reply(state,token)).await?;
            anyhow::ensure!(std::fs::read_to_string(&marker)? == format!("{contents}\n"), "Unexpected fixture edit");
        }
        let handle = connection.changes_handle()?;
        let first = handle.diff(anchors[0].clone())?.await??;
        let second = handle.diff(anchors[1].clone())?.await??;
        let first = first.iter().find(|file| file.file == "marker.txt").context("First turn has no marker diff")?;
        let second = second.iter().find(|file| file.file == "marker.txt").context("Second turn has no marker diff")?;
        anyhow::ensure!(first.patch.contains("-BASE") && first.patch.contains("+TURN_ONE") && !first.patch.contains("TURN_TWO"),
            "Earlier turn did not retain its own snapshot after the second edit");
        anyhow::ensure!(second.patch.contains("-TURN_ONE") && second.patch.contains("+TURN_TWO"),
            "Second turn did not compare its own start and end snapshots");
        let turns = handle.turns();
        anyhow::ensure!(turns.len() == 2 && turns[0].id == anchors[1] && turns[1].id == anchors[0], "Turn picker did not follow idle boundaries");
        Ok(())
    }.await;
    let interrupted = api.interrupt(&id).await;
    let removed = api.remove_test_session(&id).await;
    result?;
    interrupted?;
    removed?;
    Ok(())
}

#[tokio::test]
#[ignore = "stages a snippet in an isolated V2 session with resume=false; no model inference"]
async fn live_service_selected_snippet_admission() -> Result<()> {
    let api = Api::connect(Endpoint::discover().await?).await?;
    let directory = tempfile::Builder::new()
        .prefix("helix-snippets-")
        .tempdir_in("/tmp/opencode")?;
    let path = directory.path().join("snippet.txt");
    std::fs::write(&path, "ON_DISK_VERSION\n")?;
    let id = new_id("ses");
    api.create(&id, directory.path()).await?;
    let result: Result<()> = async {
        let snippet = snippets::Snippet {
            comment: "Explain this selected expression".into(),
            path: Some(path.clone()),
            text: "UNSAVED_SELECTED_é_😀 + %\n".into(),
            selection: snippets::Selection {
                start_line: 2,
                end_line: 2,
                start_char: 0,
                end_char: 25,
            },
        };
        let message = new_id("msg");
        let mut request = snippets::prompt(
            &message,
            "Review the attached selection",
            std::slice::from_ref(&snippet),
        )?;
        request["resume"] = json!(false);
        let admitted: Data<Value> = api
            .json(
                Method::POST,
                &format!("api/session/{id}/prompt"),
                Some(request),
            )
            .await?;
        let payload = &admitted.data["payload"];
        anyhow::ensure!(
            payload["text"].as_str().is_some_and(|text| text.starts_with("Review the attached selection\nThe user made the following comment regarding line 2 of ") && text.ends_with(": Explain this selected expression")),
            "Reference comment did not reach the model-facing text"
        );
        anyhow::ensure!(payload["metadata"]["displayText"] == "Review the attached selection"
            && payload["metadata"]["comments"][0]["comment"] == snippet.comment,
            "Service did not retain separate message and selection comment");
        anyhow::ensure!(
            payload["files"][0]["source"]["uri"] == snippet.attachment()?.source["uri"],
            "Service discarded snippet source range"
        );
        anyhow::ensure!(
            payload["metadata"]["comments"][0]["preview"] == snippet.text,
            "Service discarded desktop presentation metadata"
        );
        // Materialized bytes are base64 in the native V2 attachment projection.
        anyhow::ensure!(
            payload["files"][0]["data"] == "VU5TQVZFRF9TRUxFQ1RFRF/DqV/wn5iAICsgJQo=",
            "Service did not preserve exact UTF-8 snapshot bytes"
        );
        let inbox = api.inbox(&id).await?;
        anyhow::ensure!(
            inbox
                .iter()
                .any(|input| input.id == message && input.payload.files.len() == 1),
            "Staged snippet not retained in inbox"
        );
        anyhow::ensure!(
            api.messages(&id)
                .await?
                .iter()
                .all(|message| message.kind != "assistant"),
            "Staged attachment unexpectedly triggered inference"
        );
        anyhow::ensure!(
            std::fs::read_to_string(&path)? == "ON_DISK_VERSION\n",
            "Attachment changed the source file"
        );
        Ok(())
    }
    .await;
    let removed = api.remove_test_session(&id).await;
    result?;
    removed?;
    Ok(())
}

#[tokio::test]
#[ignore = "uses a running V2 service with an isolated session; no model inference"]
async fn live_service_model_and_variant_switching() -> Result<()> {
    let api = Api::connect(Endpoint::discover().await?).await?;
    let directory = tempfile::Builder::new()
        .prefix("helix-models-")
        .tempdir_in("/tmp/opencode")?;
    let catalog = api.models(directory.path()).await?;
    let rows = models::choices(catalog, None);
    let explicit = rows
        .iter()
        .find(|row| {
            row.model.provider == "openai"
                && row.model.model == "gpt-6-luna"
                && row.model.variant.as_deref() == Some("none")
        })
        .or_else(|| rows.iter().find(|row| row.model.variant.is_some()))
        .context("No enabled chat model with variants")?
        .model
        .clone();
    let mut default = explicit.clone();
    default.variant = None;
    let id = new_id("ses");
    api.create(&id, directory.path()).await?;
    let result: Result<()> = async {
        let connection = Connection::new(directory.path().to_owned(), Some(id.clone()));
        wait_for(&connection, |state| state.connected).await?;
        let handle = connection.model_handle()?;
        let loaded = handle.catalog()?.await??;
        anyhow::ensure!(!loaded.is_empty(), "Worker returned an empty catalog");
        for model in [explicit, default] {
            handle.switch(model.clone())?.await??;
            let actual = api.session(&id).await?.model;
            anyhow::ensure!(
                actual
                    .as_ref()
                    .is_some_and(|actual| actual.same_selection(&model)),
                "Server did not retain selected model and variant: requested {}, received {:?}",
                model.label(),
                actual
            );
            anyhow::ensure!(
                handle
                    .current()
                    .as_ref()
                    .is_some_and(|actual| actual.same_selection(&model)),
                "Chat did not refresh model metadata"
            );
        }
        // The service records model changes as control messages, without inference.
        anyhow::ensure!(
            api.messages(&id)
                .await?
                .iter()
                .all(|message| !matches!(message.kind.as_str(), "user" | "assistant")),
            "Model selection unexpectedly generated a user or assistant turn"
        );
        Ok(())
    }
    .await;
    let removed = api.remove_test_session(&id).await;
    result?;
    removed?;
    Ok(())
}

async fn fixture(api: &Api, directory: &std::path::Path) -> Result<String> {
    fixture_with_rules(
        api,
        directory,
        json!([{"action":"*", "resource":"*", "effect":"deny"}]),
    )
    .await
}

async fn fixture_with_rules(
    api: &Api,
    directory: &std::path::Path,
    permissions: Value,
) -> Result<String> {
    let mut query = reqwest::Url::parse("http://localhost/api/model")?;
    query.query_pairs_mut().append_pair(
        "location[directory]",
        directory.to_str().context("Test path is not UTF-8")?,
    );
    let models: Data<Vec<Value>> = api
        .json(
            Method::GET,
            &format!("api/model?{}", query.query().unwrap()),
            None,
        )
        .await?;
    let requested =
        std::env::var("HELIX_OPENCODE_TEST_MODEL").unwrap_or_else(|_| "opencode/big-pickle".into());
    // Paid/plan-backed inference is an explicit opt-in for the user-approved
    // low-cost Luna model, never an automatic fallback from free inference.
    let luna = requested == "openai/gpt-6-luna#none";
    let (provider, id) = if luna {
        ("openai", "gpt-6-luna")
    } else {
        (
            "opencode",
            requested.strip_prefix("opencode/").unwrap_or(&requested),
        )
    };
    let model = models
        .data
        .iter()
        .find(|model| {
            model["providerID"] == provider
                && model["enabled"] == true
                && model["id"] == id
                && model["type"] == "chat"
                && model["status"] == "active"
                && (luna || model["cost"].as_array().is_some_and(|tiers| {
                    !tiers.is_empty()
                        && tiers.iter().all(|tier| {
                            tier["input"].as_f64() == Some(0.0)
                                && tier["output"].as_f64() == Some(0.0)
                                && tier.get("cache").is_none_or(|cache| {
                                    cache.as_object().is_some_and(|prices| {
                                        prices.values().all(|v| v.as_f64() == Some(0.0))
                                    })
                                })
                        })
                }))
        })
        .context("Requested model unavailable or not explicitly zero-cost; only openai/gpt-6-luna#none permits non-free test inference")?;
    let mut reference = json!({"providerID": provider, "id": model["id"]});
    if luna {
        anyhow::ensure!(
            model["variants"]
                .as_array()
                .is_some_and(|variants| variants.iter().any(|v| v["id"] == "none")),
            "Luna's no-reasoning variant is unavailable"
        );
        reference["variant"] = json!("none");
    }
    eprintln!("Live test model: {requested}");
    let id = new_id("ses");
    let session: Data<Session> = api
        .json(
            Method::POST,
            "api/session",
            Some(json!({
                "id": id, "title": "Helix live integration test (temporary)",
                "location": {"directory": directory},
                "model": reference,
                "permissions": permissions
            })),
        )
        .await?;
    anyhow::ensure!(
        session.data.id == id,
        "Service returned a different session ID"
    );
    Ok(id)
}

#[tokio::test]
#[ignore = "uses explicit model inference and writes only an isolated fixture file"]
async fn live_service_edit_approval_question_and_diff() -> Result<()> {
    use super::interactions::PermissionDecision;
    let api = Api::connect(Endpoint::discover().await?).await?;
    let directory = tempfile::Builder::new()
        .prefix("helix-milestone-")
        .tempdir_in("/tmp/opencode")?;
    anyhow::ensure!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(directory.path())
            .status()?
            .success(),
        "Cannot initialize fixture repository"
    );
    let id = fixture_with_rules(
        &api,
        directory.path(),
        json!([
            {"action":"*","resource":"*","effect":"deny"},
            {"action":"edit","resource":"marker.txt","effect":"ask"},
            {"action":"read","resource":"marker.txt","effect":"allow"},
            {"action":"question","resource":"*","effect":"allow"}
        ]),
    )
    .await?;
    let result: Result<()> = async {
        let connection = Connection::new(directory.path().to_owned(), Some(id.clone()));
        wait_for(&connection, |state| state.connected).await?;
        let mut connection = connection;
        connection.send("This is an isolated integration test. Use write to create marker.txt containing exactly HELIX_MILESTONE followed by a newline. Then use question to ask 'Proceed to review?' with one option labeled 'Review', and wait for my answer. After I answer, reply exactly HELIX_MILESTONE_DONE. Do not use shell or modify any other file. Do not merely describe the steps.".into())?;
        wait_for(&connection, |state| !state.permissions.is_empty()
            || (!state.busy && state.pending.is_empty() && state.messages.iter().any(|m| m.kind == "assistant" && m.time.completed.is_some()))).await?;
        let permission = connection.state.lock().permissions.values().next().cloned().context("Agent did not request the fixture edit")?;
        anyhow::ensure!(permission.action == "edit" && permission.resources == ["marker.txt"], "Refusing any approval outside the single fixture file");
        anyhow::ensure!(!directory.path().join("marker.txt").exists(), "File was written before approval");
        connection.interactions().permission(permission, PermissionDecision::Once)?;
        wait_for(&connection, |state| !state.forms.is_empty()).await?;
        let form = connection.state.lock().forms.values().next().cloned().context("Agent did not ask the review question")?;
        let field = form.fields.first().context("Question has no field")?;
        anyhow::ensure!(field.kind == "string" && field.options.iter().any(|option| option.value == "Review"), "Unexpected fixture question");
        let mut answer = serde_json::Map::new();
        answer.insert(field.key.clone(), json!("Review"));
        connection.interactions().form(form, Some(answer))?;
        wait_for(&connection, |state| !state.busy && has_reply(state, "HELIX_MILESTONE_DONE")).await?;
        anyhow::ensure!(std::fs::read_to_string(directory.path().join("marker.txt"))? == "HELIX_MILESTONE\n", "Unexpected fixture file contents");
        connection.interactions().review()?;
        wait_for(&connection, |state| !state.review_loading).await?;
        let state = connection.state.lock();
        anyhow::ensure!(state.review_error.is_none(), "Review failed: {:?}", state.review_error);
        anyhow::ensure!(state.review.as_ref().is_some_and(|files| files.iter().any(|f| f.file == "marker.txt" && f.patch.contains("+HELIX_MILESTONE"))), "Session diff did not include the approved edit");
        Ok(())
    }.await;
    let interrupted = api.interrupt(&id).await;
    let removed = api.remove_test_session(&id).await;
    result?;
    interrupted?;
    removed?;
    Ok(())
}

async fn wait_for(connection: &Connection, ready: impl Fn(&State) -> bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            {
                let state = connection.state.lock();
                if let Some(error) = &state.error {
                    bail!("Live worker: {error}");
                }
                if ready(&state) {
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("Timed out waiting for the live chat worker")?
}

fn has_reply(state: &State, token: &str) -> bool {
    state.messages.iter().any(|message| {
        message.kind == "assistant"
            && message.time.completed.is_some()
            && message
                .content
                .iter()
                .any(|part| part.kind == "text" && part.text.contains(token))
    })
}

#[tokio::test]
#[ignore = "uses a running OpenCode V2 service and explicit model inference"]
async fn live_service_chat_reconnect_and_followup() -> Result<()> {
    let api = Api::connect(Endpoint::discover().await?).await?;
    let directory = tempfile::Builder::new()
        .prefix("helix-live-")
        .tempdir_in("/tmp/opencode")?;
    let id = fixture(&api, directory.path()).await?;
    let result: Result<()> = async {
        // Exercise the same discovery, snapshot, SSE, and submit path as the pane.
        let mut connection = Connection::new(directory.path().to_owned(), Some(id.clone()));
        wait_for(&connection, |state| state.connected).await?;
        connection.send("Do not use tools. Reply with exactly HELIX_LIVE_FIRST.".into())?;
        wait_for(&connection, |state| {
            !state.busy && state.pending.is_empty() && has_reply(state, "HELIX_LIVE_FIRST")
        })
        .await?;
        anyhow::ensure!(
            connection
                .state
                .lock()
                .messages
                .iter()
                .filter(|m| m.kind == "user")
                .count()
                == 1,
            "Expected one delivered user message"
        );
        anyhow::ensure!(
            connection.state.lock().text_deltas > 0,
            "The worker must consume live text deltas, not only the final snapshot"
        );
        connection.reconnect();
        wait_for(&connection, |state| {
            state.connected && has_reply(state, "HELIX_LIVE_FIRST")
        })
        .await?;
        connection.send("Do not use tools. Reply with exactly HELIX_LIVE_SECOND.".into())?;
        wait_for(&connection, |state| {
            !state.busy && state.pending.is_empty() && has_reply(state, "HELIX_LIVE_SECOND")
        })
        .await?;
        {
            let state = connection.state.lock();
            anyhow::ensure!(
                state.messages.iter().filter(|m| m.kind == "user").count() == 2,
                "Expected two delivered user messages"
            );
            anyhow::ensure!(state.notices.is_empty(), "Unexpected pending notice");
        }
        // Interrupt promptly, before this deliberately long response can grow.
        connection.send("Do not use tools. Count from 1 to 10000, one number per line.".into())?;
        wait_for(&connection, |state| state.busy).await?;
        connection.interrupt()?;
        wait_for(&connection, |state| !state.busy && state.pending.is_empty()).await?;
        let session: Data<Value> = api
            .json(Method::GET, &format!("api/session/{id}"), None)
            .await?;
        anyhow::ensure!(
            session.data["outcome"] == "interrupted",
            "Running reply was not interrupted"
        );
        drop(connection);
        api.interrupt(&id).await?; // Idle interruption must also succeed.
        Ok(())
    }
    .await;
    // Clean up only the session created by this test, including on failure.
    let interrupted = api.interrupt(&id).await;
    let removed = api.remove_test_session(&id).await;
    result?;
    interrupted?;
    removed?;
    Ok(())
}

#[tokio::test]
#[ignore = "uses a running OpenCode V2 service; no model inference"]
async fn live_service_connection_snapshot_forms_and_reconnect() -> Result<()> {
    let api = Api::connect(Endpoint::discover().await?).await?;
    let directory = tempfile::Builder::new()
        .prefix("helix live ")
        .tempdir_in("/tmp/opencode")?;
    // Use the production create/discover path, not the test fixture's custom payload.
    let connection = Connection::new(directory.path().to_owned(), None);
    let id = connection.id.clone();
    let result: Result<()> = async {
        let mut connection = connection;
        wait_for(&connection, |state| state.connected).await?;
        anyhow::ensure!(
            connection
                .state
                .lock()
                .session
                .as_ref()
                .unwrap()
                .location
                .directory
                == directory.path(),
            "Session location changed"
        );
        anyhow::ensure!(
            api.inbox(&id).await?.is_empty(),
            "New session inbox is not empty"
        );
        anyhow::ensure!(
            api.messages(&id).await?.is_empty(),
            "New session history is not empty"
        );
        anyhow::ensure!(
            api.message(&id, &new_id("msg")).await?.is_none(),
            "Missing message should return None"
        );
        let form: Data<Value> = api
            .json(
                Method::POST,
                &format!("api/session/{id}/form"),
                Some(json!({
                    "title": "Helix test question", "fields": [{"key": "answer", "type": "string"}]
                })),
            )
            .await?;
        let form_id = form.data["id"].as_str().context("Missing test form ID")?;
        wait_for(&connection, |state| state.notices.contains_key(form_id)).await?;
        let message_id = new_id("msg");
        api.json::<Value>(
            Method::POST,
            &format!("api/session/{id}/prompt"),
            Some(json!({
                "id": message_id, "text": "Helix staged inbox test; no execution", "resume": false
            })),
        )
        .await?;
        connection.reconnect();
        wait_for(&connection, |state| {
            state.connected
                && state.notices.contains_key(form_id)
                && state
                    .pending
                    .iter()
                    .any(|input| input.id == message_id && input.accepted && !input.failed)
        })
        .await?;
        api.interrupt(&id).await?;
        anyhow::ensure!(
            connection.state.lock().connected,
            "Idle interrupt disconnected the worker"
        );
        anyhow::ensure!(
            connection.state.lock().messages.is_empty(),
            "Staging a prompt unexpectedly executed it"
        );
        Ok(())
    }
    .await;
    let removed = api.remove_test_session(&id).await;
    result?;
    removed?;
    Ok(())
}

#[tokio::test]
#[ignore = "uses a running V2 service with isolated requests; no model inference"]
async fn live_service_permissions_forms_reconnect_and_other_client_resolution() -> Result<()> {
    use super::interactions::{Form, PermissionDecision, ReplyStatus};
    let api = Api::connect(Endpoint::discover().await?).await?;
    let directory = tempfile::Builder::new()
        .prefix("helix-actions-")
        .tempdir_in("/tmp/opencode")?;
    let id = new_id("ses");
    api.json::<Data<Session>>(Method::POST, "api/session", Some(json!({
        "id":id, "title":"Helix isolated request test", "location":{"directory":directory.path()},
        "permissions":[
            {"action":"*","resource":"*","effect":"deny"},
            {"action":"helix.test","resource":"*","effect":"ask"}
        ]
    }))).await?;
    let result: Result<()> = async {
        let mut connection = Connection::new(directory.path().to_owned(), Some(id.clone()));
        wait_for(&connection, |state| state.connected).await?;
        let per_id = new_id("per");
        let created: Data<Value> = api.json(Method::POST, &format!("api/session/{id}/permission"), Some(json!({
            "id":per_id, "action":"helix.test", "resources":[directory.path().join("fixture.txt")],
            "metadata":{"note":"Fixture only. No tool is executed."}
        }))).await?;
        anyhow::ensure!(created.data["effect"] == "ask", "Fixture did not create a pending permission");
        let form: Data<Form> = api.json(Method::POST, &format!("api/session/{id}/form"), Some(json!({
            "title":"Helix fixture question", "fields":[
                {"key":"choice","type":"string","options":[{"value":"review","label":"Review"}],"required":true},
                {"key":"confirm","type":"boolean","default":false}
            ]
        }))).await?;
        let form_id = form.data.id.clone();
        wait_for(&connection, |state| state.permissions.contains_key(&per_id) && state.forms.contains_key(&form_id)).await?;
        connection.reconnect();
        wait_for(&connection, |state| state.connected && state.permissions.contains_key(&per_id) && state.forms.contains_key(&form_id)).await?;
        let expected = connection.state.lock().permissions[&per_id].clone();
        connection.interactions().permission(expected, PermissionDecision::Once)?;
        wait_for(&connection, |state| !state.permissions.contains_key(&per_id) && state.resolving.is_empty()).await?;

        // A second client answers first. Neither stale UI data nor a repeated
        // request may send an answer to some other question.
        let stale = connection.state.lock().forms[&form_id].clone();
        let answer = json!({"choice":"review","confirm":true}).as_object().unwrap().clone();
        anyhow::ensure!(matches!(api.form_reply(&id, &form_id, Some(&answer)).await?, ReplyStatus::Applied), "External answer did not settle the form");
        wait_for(&connection, |state| !state.forms.contains_key(&form_id)).await?;
        anyhow::ensure!(connection.interactions().form(stale, Some(answer.clone())).is_err(), "A settled question was resubmitted");
        anyhow::ensure!(matches!(api.form_reply(&id, &form_id, Some(&answer)).await?, ReplyStatus::Settled), "Duplicate answer was not reported as settled");

        let form: Data<Form> = api.json(Method::POST, &format!("api/session/{id}/form"), Some(json!({
            "title":"Answer from Helix", "fields":[
                {"key":"number","type":"integer","minimum":1,"maximum":3,"required":true},
                {"key":"code","type":"string","pattern":"^[A-Z]+$","required":true}
            ]
        }))).await?;
        let form_id = form.data.id.clone();
        wait_for(&connection, |state| state.forms.contains_key(&form_id)).await?;
        let expected = connection.state.lock().forms[&form_id].clone();
        connection.interactions().form(expected.clone(), Some(json!({"number":2,"code":"invalid"}).as_object().unwrap().clone()))?;
        wait_for(&connection, |state| state.resolving.is_empty()).await?;
        anyhow::ensure!(connection.state.lock().forms.contains_key(&form_id), "Server validation incorrectly settled the form");
        connection.interactions().form(expected, Some(json!({"number":2,"code":"VALID"}).as_object().unwrap().clone()))?;
        wait_for(&connection, |state| !state.forms.contains_key(&form_id) && state.resolving.is_empty()).await?;
        anyhow::ensure!(connection.state.lock().connected, "Answering a request disconnected the worker");
        Ok(())
    }.await;
    let removed = api.remove_test_session(&id).await;
    result?;
    removed?;
    Ok(())
}
