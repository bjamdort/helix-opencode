use super::*;
use crate::{commands::pane::PaneCommand, compositor};
use anyhow::Result;

impl OpenCode {
    pub(super) fn update_snippet_comment(
        &mut self,
        index: usize,
        expected: &opencode::snippets::Snippet,
        comment: String,
    ) -> Result<()> {
        anyhow::ensure!(
            comment.len() <= opencode::snippets::MAX_COMMENT_BYTES,
            "Selection comments exceed the 16 KiB limit"
        );
        anyhow::ensure!(
            self.snippets.get(index) == Some(expected),
            "Selection changed; reopen :opencode-comment"
        );
        let total: usize = self
            .snippets
            .iter()
            .map(|snippet| snippet.text.len() + snippet.comment.len())
            .sum();
        anyhow::ensure!(
            total - expected.comment.len() + comment.len() <= opencode::snippets::MAX_BYTES,
            "Staged snippets exceed the 1 MiB limit"
        );
        self.snippets[index].comment = comment;
        Ok(())
    }

    pub(super) fn stage_snippets(
        &mut self,
        snippets: Vec<opencode::snippets::Snippet>,
        message: &str,
    ) -> Result<()> {
        let mut staged = self.snippets.clone();
        for snippet in snippets {
            if let Some(existing) = staged
                .iter_mut()
                .find(|existing| existing.same_source(&snippet))
            {
                if !snippet.comment.is_empty() {
                    existing.comment = snippet.comment;
                }
            } else {
                staged.push(snippet);
            }
        }
        anyhow::ensure!(
            staged.len() <= opencode::snippets::MAX_SNIPPETS,
            "Too many staged snippets; clear context first"
        );
        anyhow::ensure!(
            staged
                .iter()
                .map(|snippet| snippet.text.len() + snippet.comment.len())
                .sum::<usize>()
                <= opencode::snippets::MAX_BYTES,
            "Staged snippets exceed the 1 MiB limit; clear context first"
        );
        anyhow::ensure!(
            staged
                .iter()
                .all(|snippet| snippet.comment.len() <= opencode::snippets::MAX_COMMENT_BYTES),
            "Selection comments exceed the 16 KiB limit"
        );
        self.snippets = staged;
        if !message.is_empty() {
            let mut draft = self.draft.lock();
            if !draft.is_empty() {
                draft.push(' ');
            }
            draft.push_str(message);
        }
        Ok(())
    }
}

pub(crate) fn selection_callback(
    editor: &Editor,
    message: String,
    comment: String,
) -> Result<compositor::Callback> {
    anyhow::ensure!(
        comment.len() <= opencode::snippets::MAX_COMMENT_BYTES,
        "Selection comments exceed the 16 KiB limit"
    );
    let snippets = opencode::snippets::capture(editor)?;
    let directory = helix_view::doc!(editor).workspace_root().to_owned();
    Ok(Box::new(move |compositor, cx| {
        let prompt = crate::ui::Prompt::new(
            "Selection comment > ".into(),
            None,
            |_, _| Vec::new(),
            |_, _, _| {},
        )
        .with_line(comment, cx.editor)
        .with_validate_callback(move |_, comment| {
            let mut snippets = snippets.clone();
            for snippet in &mut snippets {
                snippet.comment = comment.trim().to_owned();
            }
            Some(stage_callback(directory.clone(), snippets, message.clone()))
        });
        compositor.push(Box::new(prompt));
    }))
}

fn stage_callback(
    directory: PathBuf,
    snippets: Vec<opencode::snippets::Snippet>,
    message: String,
) -> compositor::Callback {
    Box::new(move |compositor, cx| {
        if snippets
            .iter()
            .any(|snippet| snippet.comment.len() > opencode::snippets::MAX_COMMENT_BYTES)
        {
            cx.editor
                .set_error("Selection comments exceed the 16 KiB limit");
            return;
        }
        let result = (|| -> Result<_> {
            let view = compositor
                .find::<crate::ui::EditorView>()
                .ok_or_else(|| anyhow::anyhow!("Editor view is unavailable"))?;
            view.open_opencode(directory, None, cx.editor)?;
            let id = view.panes.find(ID).unwrap();
            view.panes
                .get_mut::<OpenCode>(id)
                .unwrap()
                .stage_snippets(snippets, &message)?;
            Ok(view.panes.command(PaneCommand::Compose, 1, cx.editor))
        })();
        match result {
            Ok(Some(callback)) => callback(compositor, cx),
            Ok(None) => (),
            Err(error) => cx.editor.set_error(error.to_string()),
        }
    })
}

/// Edit one staged reference, without touching the main chat draft or sending it.
pub(crate) fn comment_callback() -> compositor::Callback {
    Box::new(|compositor, cx| {
        let result = (|| -> Result<_> {
            let view = compositor
                .find::<crate::ui::EditorView>()
                .ok_or_else(|| anyhow::anyhow!("Editor view is unavailable"))?;
            let id = view
                .panes
                .find(ID)
                .ok_or_else(|| anyhow::anyhow!("Open a chat with :opencode first"))?;
            let pane = view.panes.get_mut::<OpenCode>(id).unwrap();
            anyhow::ensure!(
                !pane.snippets.is_empty(),
                "Attach a selection with :opencode-selection first"
            );
            Ok((
                id,
                pane.draft.clone(),
                pane.snippets
                    .iter()
                    .cloned()
                    .enumerate()
                    .collect::<Vec<_>>(),
            ))
        })();
        let (id, draft, snippets) = match result {
            Ok(value) => value,
            Err(error) => {
                cx.editor.set_error(error.to_string());
                return;
            }
        };
        let picker = crate::ui::Picker::new(
            [
                crate::ui::PickerColumn::new(
                    "selection",
                    |(_, snippet): &(usize, opencode::snippets::Snippet), _| snippet.label().into(),
                ),
                crate::ui::PickerColumn::new(
                    "comment",
                    |(_, snippet): &(usize, opencode::snippets::Snippet), _| {
                        snippet.comment.as_str().into()
                    },
                ),
            ],
            0,
            snippets,
            (),
            |_, _, _| {},
        )
        .with_compositor_callback(move |_, (index, snapshot), _| {
            let index = *index;
            let snapshot = snapshot.clone();
            let draft = draft.clone();
            Some(Box::new(move |compositor, cx| {
                let initial = snapshot.comment.clone();
                let prompt = crate::ui::Prompt::new(
                    "Selection comment > ".into(),
                    None,
                    |_, _| Vec::new(),
                    |_, _, _| {},
                )
                .with_line(initial, cx.editor)
                .with_validate_callback(move |_, comment| {
                    let comment = comment.trim().to_owned();
                    let draft = draft.clone();
                    let snapshot = snapshot.clone();
                    Some(Box::new(move |compositor, cx| {
                        let result = (|| -> Result<()> {
                            let pane = compositor
                                .find::<crate::ui::EditorView>()
                                .and_then(|view| view.panes.get_mut::<OpenCode>(id))
                                .filter(|pane| Arc::ptr_eq(&draft, &pane.draft))
                                .ok_or_else(|| {
                                    anyhow::anyhow!("Chat changed; reopen :opencode-comment")
                                })?;
                            pane.update_snippet_comment(index, &snapshot, comment)
                        })();
                        match result {
                            Ok(()) => cx
                                .editor
                                .set_status("Selection comment updated; compose a message to send"),
                            Err(error) => cx.editor.set_error(error.to_string()),
                        }
                    }))
                });
                compositor.push(Box::new(prompt));
            }))
        });
        compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
    })
}

pub(crate) fn clear_callback() -> compositor::Callback {
    Box::new(|compositor, cx| {
        let pane = compositor.find::<crate::ui::EditorView>().and_then(|view| {
            view.panes
                .find(ID)
                .and_then(|id| view.panes.get_mut::<OpenCode>(id))
        });
        if let Some(pane) = pane {
            pane.snippets.clear();
            cx.editor.set_status("OpenCode snippet context cleared");
        } else {
            cx.editor.set_error("Open a chat with :opencode first");
        }
    })
}

#[cfg(test)]
mod tests {
    use helix_core::command_line::Args;

    #[test]
    fn selection_comment_flag_is_distinct_from_the_overall_message() {
        let command = crate::commands::typed::TYPABLE_COMMAND_LIST
            .iter()
            .find(|command| command.name == "opencode-selection")
            .unwrap();
        for input in [
            r#"--comment "Why can this return null?" Review the error handling"#,
            r#"-c "Why can this return null?" Review the error handling"#,
        ] {
            let args =
                Args::parse(input, command.signature, true, |token| Ok(token.content)).unwrap();
            assert_eq!(args.get_flag("comment"), Some("Why can this return null?"));
            assert_eq!(args.join(" "), "Review the error handling");
        }
        assert!(
            Args::parse("--comment", command.signature, true, |token| Ok(
                token.content
            ))
            .is_err()
        );
        assert!(!crate::commands::pane::typed_is_workspace(
            "opencode-selection"
        ));
        assert!(crate::commands::pane::typed_is_workspace(
            "opencode-comment"
        ));
    }
}
