//! Selecting a session is explicit; loading and cancellation never replace chat.
use super::*;
use crate::{
    compositor,
    job::{Callback, Job},
    opencode::{sessions::SessionBrowser, Session},
    ui::{overlay::overlaid, Picker, PickerColumn, Prompt},
};
use anyhow::{Context as _, Result};
use std::cell::Cell;

#[derive(Clone)]
struct Origin {
    id: PaneId,
    session_id: String,
    draft: Arc<Mutex<String>>,
}

impl Origin {
    fn matches(&self, expected: &Self) -> bool {
        self.id == expected.id
            && self.session_id == expected.session_id
            && Arc::ptr_eq(&self.draft, &expected.draft)
    }
}

#[derive(Clone)]
struct Choice {
    session: Session,
    browser: SessionBrowser,
    label: String,
    current: bool,
}

fn label(session: &Session) -> String {
    let title = session
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("Untitled session");
    let text = format!(
        "{title}  ·  {}  ·  {}",
        session.location.directory.display(),
        session.id
    );
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn columns() -> [PickerColumn<Choice, ()>; 2] {
    [
        PickerColumn::new("session", |choice: &Choice, _| choice.label.as_str().into()),
        PickerColumn::new("selected", |choice: &Choice, _| {
            if choice.current { "current" } else { "" }.into()
        }),
    ]
}

impl OpenCode {
    pub(super) fn needs_session_confirmation(&self, selected: &str) -> bool {
        self.session_id != selected && (!self.draft.lock().is_empty() || !self.snippets.is_empty())
    }
}

fn origin(view: &mut crate::ui::EditorView) -> Option<Origin> {
    let id = view.panes.find(ID)?;
    let pane = view.panes.get_mut::<OpenCode>(id)?;
    Some(Origin {
        id,
        session_id: pane.session_id.clone(),
        draft: pane.draft.clone(),
    })
}

fn unchanged(view: &mut crate::ui::EditorView, expected: &Option<Origin>) -> bool {
    match (origin(view), expected) {
        (None, None) => true,
        (Some(actual), Some(expected)) => actual.matches(expected),
        _ => false,
    }
}

fn attach_callback(
    session: Session,
    expected: Option<Origin>,
    discard: bool,
) -> compositor::Callback {
    Box::new(move |compositor, cx| attach(cx.editor, compositor, session, expected, discard))
}

fn attach(
    editor: &mut Editor,
    compositor: &mut compositor::Compositor,
    session: Session,
    expected: Option<Origin>,
    discard: bool,
) {
    let result = (|| -> Result<bool> {
        let view = compositor
            .find::<crate::ui::EditorView>()
            .context("Editor view is unavailable")?;
        anyhow::ensure!(
            unchanged(view, &expected),
            "Chat changed; reopen :opencode-sessions"
        );
        if let Some(id) = view.panes.find(ID) {
            let pane = view.panes.get_mut::<OpenCode>(id).unwrap();
            if !discard && pane.needs_session_confirmation(&session.id) {
                return Ok(false);
            }
        }
        view.open_opencode(
            session.location.directory.clone(),
            Some(session.id.clone()),
            editor,
        )?;
        Ok(true)
    })();
    match result {
        Ok(true) => (),
        Ok(false) => {
            let prompt = Prompt::new(
                "Switch chat and discard its unsent draft/context? Type yes > ".into(),
                None,
                |_, _| Vec::new(),
                |_, _, _| {},
            )
            .with_validate_callback(move |_, answer| {
                if answer.trim() == "yes" {
                    Some(attach_callback(session.clone(), expected.clone(), true))
                } else {
                    Some(Box::new(|_, cx| {
                        cx.editor.set_status("Session switch cancelled")
                    }))
                }
            });
            compositor.push(Box::new(prompt));
        }
        Err(error) => editor.set_error(error.to_string()),
    }
}

pub(crate) fn callback() -> compositor::Callback {
    Box::new(|compositor, cx| {
        let Some(view) = compositor.find::<crate::ui::EditorView>() else {
            cx.editor.set_error("Editor view is unavailable");
            return;
        };
        let expected = origin(view);
        let current = expected.as_ref().map(|origin| origin.session_id.clone());
        let (matcher, injector) = Picker::stream(columns(), ());
        let stream = injector.clone();
        let submitted = Cell::new(false);
        let picker = Picker::with_stream(matcher, 0, injector, |_, _, _| {})
            .truncate_start(false)
            .with_compositor_callback(move |_, choice, _| {
                if submitted.replace(true) {
                    return None;
                }
                let choice = choice.clone();
                let expected = expected.clone();
                Some(Box::new(move |_, cx| {
                    // A session may have moved or been deleted after listing. Check
                    // it before replacing the existing pane, and use its fresh root.
                    cx.jobs.add(Job::with_callback(async move {
                        let session = choice.browser.session(&choice.session.id).await?;
                        anyhow::ensure!(
                            session.id == choice.session.id,
                            "OpenCode returned a different session"
                        );
                        Ok(Callback::EditorCompositor(Box::new(
                            move |editor, compositor| {
                                attach(editor, compositor, session, expected, false);
                            },
                        )))
                    }));
                }))
            });
        compositor.push(Box::new(overlaid(picker)));
        cx.jobs.add(Job::with_callback(async move {
            let result: Result<usize> = async {
                if stream.is_shutdown() {
                    return Ok(0);
                }
                let browser = SessionBrowser::connect().await?;
                browser
                    .stream(
                        |session| {
                            let row = Choice {
                                current: current.as_ref() == Some(&session.id),
                                label: label(&session),
                                session,
                                browser: browser.clone(),
                            };
                            stream.push(row).is_ok()
                        },
                        || stream.is_shutdown(),
                    )
                    .await
            }
            .await;
            if stream.is_shutdown() {
                return Ok(Callback::Editor(Box::new(|_| {})));
            }
            let count = result?;
            anyhow::ensure!(count > 0, "No non-archived root sessions are available");
            Ok(Callback::Editor(Box::new(|_| {})))
        }));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_switch_guards_detect_new_chats_even_when_the_pane_and_draft_are_reused() {
        let original = Origin {
            id: PaneId::default(),
            session_id: "ses_original".into(),
            draft: Arc::new(Mutex::new(String::new())),
        };
        assert!(original.matches(&original.clone()));
        let mut new_chat = original.clone();
        new_chat.session_id = "ses_new".into();
        assert!(!new_chat.matches(&original));
        let mut replaced = original.clone();
        replaced.draft = Arc::new(Mutex::new(String::new()));
        assert!(!replaced.matches(&original));
    }

    #[test]
    fn row_labels_include_title_directory_and_id_without_terminal_controls() {
        let session: Session = serde_json::from_value(serde_json::json!({
            "id":"ses_example", "title":"Investigate\nUnicode é界\u{1b}[31m", "time":{},
            "location":{"directory":"/work/project"}
        }))
        .unwrap();
        let text = label(&session);
        assert!(text.contains("Investigate"));
        assert!(text.contains("Unicode é界"));
        assert!(text.contains("/work/project"));
        assert!(text.contains("ses_example"));
        assert!(!text.chars().any(char::is_control));
    }

    #[tokio::test]
    async fn session_streaming_injector_stops_after_picker_cancellation() {
        let (matcher, injector) = Picker::stream(
            [PickerColumn::new("session", |session: &Session, _| {
                label(session).into()
            })],
            (),
        );
        let stream = injector.clone();
        assert!(!stream.is_shutdown());
        let picker = Picker::with_stream(matcher, 0, injector, |_, _, _| {
            panic!("Cancellation must not attach a chat")
        });
        drop(picker);
        assert!(stream.is_shutdown());
    }
}
