//! Read-only turn diffs through the existing authenticated connection.
use super::*;
use interactions::FileChange;
use tokio::sync::oneshot;

#[derive(Clone)]
pub(crate) struct ChangesHandle {
    pub state: Arc<Mutex<State>>,
    requests: mpsc::Sender<Request>,
}

#[derive(Clone, Debug)]
pub(crate) struct Turn {
    pub id: String,
    pub label: String,
    pub directory: PathBuf,
}

impl Connection {
    pub fn changes_handle(&self) -> Result<ChangesHandle> {
        anyhow::ensure!(
            self.state.lock().connected,
            "OpenCode is not connected; reconnect first"
        );
        Ok(ChangesHandle {
            state: self.state.clone(),
            requests: self.requests.clone(),
        })
    }

    pub fn matches_changes_handle(&self, handle: &ChangesHandle) -> bool {
        Arc::ptr_eq(&self.state, &handle.state) && self.requests.same_channel(&handle.requests)
    }
}

impl ChangesHandle {
    pub fn diff(&self, from: String) -> Result<oneshot::Receiver<Result<Vec<FileChange>>>> {
        anyhow::ensure!(valid_id(&from, "msg"), "Invalid user message ID");
        let (reply, receiver) = oneshot::channel();
        self.requests
            .try_send(Request::TurnChanges { from, reply })
            .map_err(|_| {
                anyhow::anyhow!("OpenCode connection changed; reopen :opencode-changes")
            })?;
        Ok(receiver)
    }

    pub fn turns(&self) -> Vec<Turn> {
        turns(&self.state.lock())
    }
}

fn turns(state: &State) -> Vec<Turn> {
    let Some(session) = &state.session else {
        return Vec::new();
    };
    let messages = chronological(&state.messages);
    // A location switch records the old root as well as the new one.
    let mut directory = messages
        .iter()
        .find(|message| message.kind == "location-switched")
        .and_then(|message| message.previous.as_ref())
        .and_then(|previous| previous.get("location"))
        .and_then(|location| location.get("directory"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| session.location.directory.clone());
    let legacy = !messages.iter().any(|message| message.kind == "idle");
    let mut open = false;
    let mut turns = Vec::new();
    for message in messages {
        if message.kind == "idle" {
            open = false;
        }
        if message.kind == "location-switched" {
            if let Some(location) = &message.location {
                directory = location.directory.clone();
            }
            open = false;
        }
        if message.kind != "user" || open && !legacy {
            continue;
        }
        let text = message
            .metadata
            .as_ref()
            .and_then(|m| m.get("displayText"))
            .and_then(Value::as_str)
            .unwrap_or(&message.text)
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(120)
            .collect::<String>();
        turns.push(Turn {
            id: message.id.clone(),
            label: if text.is_empty() {
                "Attached selection".into()
            } else {
                text
            },
            directory: directory.clone(),
        });
        open = true;
    }
    turns
}

/// Messages in the order they were created. Live updates append messages as
/// they arrive, so a delivered prompt can follow the replies it started.
/// Messages without a creation time keep their place after the previous one.
fn chronological(messages: &[Message]) -> Vec<&Message> {
    let mut last = 0;
    let mut ordered: Vec<_> = messages
        .iter()
        .map(|message| {
            last = message.time.created.unwrap_or(last);
            (last, message)
        })
        .collect();
    ordered.sort_by_key(|(created, _)| *created);
    ordered.into_iter().map(|(_, message)| message).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn state(messages: Value) -> State {
        State {
            session: Some(
                serde_json::from_value(
                    json!({"id":"ses_test","location":{"directory":"/new"},"time":{}}),
                )
                .unwrap(),
            ),
            messages: serde_json::from_value(messages).unwrap(),
            ..State::default()
        }
    }

    #[test]
    fn turn_picker_groups_steering_prompts_and_preserves_each_turn_location() {
        let state = state(json!([
            {"id":"msg_first","type":"user","text":"First turn"},
            {"id":"msg_steer","type":"user","text":"Steering, not a separate turn"},
            {"id":"msg_idle","type":"idle"},
            {"id":"msg_move","type":"location-switched","location":{"directory":"/new"},"previous":{"location":{"directory":"/old"}}},
            {"id":"msg_second","type":"user","text":"Second turn"},
            {"id":"msg_idle2","type":"idle"}
        ]));
        let result = turns(&state);
        assert_eq!(
            result
                .iter()
                .map(|turn| turn.id.as_str())
                .collect::<Vec<_>>(),
            ["msg_first", "msg_second"]
        );
        assert_eq!(result[0].directory, Path::new("/old"));
        assert_eq!(result[1].directory, Path::new("/new"));
    }

    #[test]
    fn turns_follow_creation_time_when_live_updates_arrive_out_of_order() {
        let state = state(json!([
            {"id":"msg_reply","type":"assistant","time":{"created":20}},
            {"id":"msg_idle","type":"idle","time":{"created":30}},
            {"id":"msg_late","type":"user","text":"Latest","time":{"created":40}},
            {"id":"msg_first","type":"user","text":"First","time":{"created":10}},
            {"id":"msg_streamed","type":"assistant"}
        ]));
        assert_eq!(
            turns(&state)
                .iter()
                .map(|turn| turn.id.as_str())
                .collect::<Vec<_>>(),
            ["msg_first", "msg_late"]
        );
    }

    #[test]
    fn legacy_histories_without_idle_markers_offer_each_user_prompt() {
        let state = state(json!([
            {"id":"msg_first","type":"user","text":"First"},
            {"id":"msg_second","type":"user","text":"Second"}
        ]));
        assert_eq!(turns(&state).len(), 2);
    }
}
