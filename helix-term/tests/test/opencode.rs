use super::*;
use helix_core::Position;
use helix_view::doc;

#[tokio::test(flavor = "multi_thread")]
async fn bufferline_is_clipped_to_the_editor_and_does_not_reserve_a_row_above_tools(
) -> anyhow::Result<()> {
    use helix_term::ui::EditorView;
    use helix_view::{
        editor::BufferLine,
        graphics::{Rect, Style},
    };
    use tui::buffer::Buffer;
    let root = tempfile::tempdir()?;
    let path = root
        .path()
        .join("a-very-long-buffer-name-to-test-clipping.rs");
    std::fs::write(&path, "original\n")?;
    let mut config = helpers::test_config();
    config.editor.bufferline = BufferLine::Always;
    let mut app = AppBuilder::new()
        .with_file(path, None)
        .with_config(config)
        .build()?;
    let original_y = std::cell::Cell::new(0);
    test_key_sequences(
        &mut app,
        vec![
            (
                None,
                Some(&|app| {
                    original_y.set(app.editor.tree.views().next().unwrap().0.area.y);
                    let outer = Rect::new(0, 0, 60, 5);
                    for viewport in [
                        Rect::new(12, 2, 15, 1),
                        Rect::new(12, 2, 0, 1),
                        Rect::new(12, 2, 15, 0),
                    ] {
                        let mut surface = Buffer::empty(outer);
                        for y in 0..outer.height {
                            surface.set_string(
                                0,
                                y,
                                &"x".repeat(outer.width as usize),
                                Style::default(),
                            );
                        }
                        EditorView::render_bufferline(&app.editor, viewport, &mut surface);
                        for y in 0..outer.height {
                            for x in 0..outer.width {
                                if x < viewport.x
                                    || x >= viewport.right()
                                    || y < viewport.y
                                    || y >= viewport.bottom()
                                {
                                    assert_eq!(
                                        surface.get(x, y).unwrap().symbol.as_str(),
                                        "x",
                                        "bufferline overflow at {x},{y} in {viewport:?}"
                                    );
                                }
                            }
                        }
                    }
                }),
            ),
            (
                Some(" .:opencode ses_bufferline_test<ret>"),
                Some(&|app| {
                    let area = app.editor.tree.views().next().unwrap().0.area;
                    assert_eq!(area.y, original_y.get());
                    assert!(area.x > 0);
                    assert_eq!(doc!(app.editor).text().to_string(), "original\n");
                }),
            ),
            (Some(":qa!<ret>"), None),
        ],
        true,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn nested_panes_and_live_layout_changes_preserve_dirty_buffers_and_splits(
) -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("source.rs");
    std::fs::write(&path, "original\n")?;
    let mut app = AppBuilder::new().with_file(path.clone(), None).build()?;
    let views = std::cell::RefCell::new(Vec::new());
    test_key_sequences(
        &mut app,
        vec![
            (
                Some("iunsaved <esc>:vsplit<ret>"),
                Some(&|app| {
                    *views.borrow_mut() =
                        app.editor.tree.views().map(|(view, _)| view.id).collect();
                    assert_eq!(views.borrow().len(), 2);
                    assert!(doc!(app.editor).is_modified());
                }),
            ),
            (
                Some(" .:opencode ses_layout_test<ret>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                    assert_eq!(
                        app.editor
                            .tree
                            .views()
                            .map(|(view, _)| view.id)
                            .collect::<Vec<_>>(),
                        *views.borrow()
                    );
                    assert_eq!(doc!(app.editor).text().to_string(), "unsaved original\n");
                }),
            ),
            (
                Some(":set-option panes.opencode-position right<ret>"),
                Some(&|app| {
                    assert_eq!(
                        app.editor.config().panes.opencode_position,
                        helix_view::editor::PanePosition::Right
                    );
                }),
            ),
            (
                Some(":set-option panes.file-explorer-position right<ret>"),
                None,
            ),
            (
                Some(":set-option panes.opencode-size 30<ret>"),
                Some(&|app| {
                    use helix_view::editor::PanePosition;
                    assert_eq!(
                        app.editor.config().panes.opencode_position,
                        PanePosition::Right
                    );
                    assert_eq!(
                        app.editor.config().panes.file_explorer_position,
                        PanePosition::Right
                    );
                    assert_eq!(app.editor.config().panes.opencode_size, 30);
                    assert_eq!(
                        app.editor
                            .tree
                            .views()
                            .map(|(view, _)| view.id)
                            .collect::<Vec<_>>(),
                        *views.borrow()
                    );
                    assert!(doc!(app.editor).is_modified());
                }),
            ),
            (
                Some(":set-option panes.opencode-size 0<ret>"),
                Some(&|app| {
                    assert_eq!(app.editor.config().panes.opencode_size, 30);
                }),
            ),
            (
                Some(":q<ret> .:q<ret>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_none());
                    assert_eq!(
                        app.editor
                            .tree
                            .views()
                            .map(|(view, _)| view.id)
                            .collect::<Vec<_>>(),
                        *views.borrow()
                    );
                    assert_eq!(doc!(app.editor).text().to_string(), "unsaved original\n");
                }),
            ),
            (Some(":qa!<ret>"), None),
        ],
        true,
    )
    .await?;
    assert_eq!(std::fs::read_to_string(path)?, "original\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn session_picker_cancel_keeps_documents_and_existing_chat_unchanged() -> anyhow::Result<()> {
    let mut app = AppBuilder::new().with_input_text("#[hello|]#").build()?;
    let original_chat = std::cell::Cell::new(None);
    test_key_sequences(
        &mut app,
        vec![
            (
                Some(":opencode-sessions<ret>"),
                Some(&|app| {
                    assert!(
                        app.editor.focused_pane.is_none(),
                        "Browsing must not create a chat pane"
                    );
                    assert_eq!(doc!(app.editor).text().to_string(), "hello");
                }),
            ),
            (
                Some("search text<esc>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_none());
                    assert_eq!(doc!(app.editor).text().to_string(), "hello");
                }),
            ),
            (
                Some(":opencode ses_picker_test<ret>iUnsent draft<esc>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                    original_chat.set(app.editor.focused_pane);
                }),
            ),
            (
                Some(":opencode-sessions<ret>search text<esc>"),
                Some(&|app| {
                    assert_eq!(app.editor.focused_pane, original_chat.get());
                    assert_eq!(doc!(app.editor).text().to_string(), "hello");
                }),
            ),
            (Some(":qa!<ret>"), None),
        ],
        true,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn git_review_opens_the_buffer_with_inline_changes_and_restores_it_on_close(
) -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let git = |args: &[&str]| -> anyhow::Result<()> {
        let output = std::process::Command::new("git")
            .current_dir(root.path())
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
        anyhow::ensure!(output.status.success(), "Fixture Git failed");
        Ok(())
    };
    git(&["init", "--quiet"])?;
    let path = root.path().join("source.rs");
    std::fs::write(&path, "original\n")?;
    std::fs::write(root.path().join("clean.rs"), "clean\n")?;
    git(&["add", "."])?;
    git(&["commit", "--quiet", "-m", "initial"])?;
    let mut app = AppBuilder::new().with_file(path.clone(), None).build()?;
    {
        let (view, doc) = helix_view::current!(app.editor);
        let transaction = helix_core::Transaction::change_by_selection(
            doc.text(),
            &helix_core::Selection::point(0),
            |_| (0, 0, Some("unsaved ".into())),
        );
        doc.apply(&transaction, view.id);
    }
    std::fs::write(&path, "disk change\n")?;
    let tree = std::cell::Cell::new(None);
    test_key_sequences(
        &mut app,
        vec![
            (
                Some(":git-changes<ret>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                    tree.set(app.editor.focused_pane);
                    assert_eq!(doc!(app.editor).text().to_string(), "unsaved original\n");
                }),
            ),
            (
                Some("<ret>"),
                Some(&|app| {
                    assert!(
                        app.editor.focused_pane.is_none(),
                        "The change opens as an editable buffer"
                    );
                    let doc = doc!(app.editor);
                    assert_eq!(doc.path(), Some(path.as_path()));
                    assert!(doc.inline_diff);
                    assert_eq!(doc.diff_source(), None, "Git review compares with HEAD");
                    assert_eq!(
                        doc.text().to_string(),
                        "unsaved original\n",
                        "Opening keeps the unsaved buffer"
                    );
                    assert!(doc.is_modified());
                }),
            ),
            (
                Some("Ax<esc>"),
                Some(&|app| {
                    assert_eq!(doc!(app.editor).text().to_string(), "unsaved originalx\n");
                }),
            ),
            (
                Some(":changes-close<ret>"),
                Some(&|app| {
                    assert_eq!(
                        app.editor.focused_pane,
                        tree.get(),
                        "Closing review restores the same explorer"
                    );
                    let doc = app
                        .editor
                        .documents()
                        .find(|doc| doc.path() == Some(path.as_path()))
                        .unwrap();
                    assert!(!doc.inline_diff, "Leaving review hides inline changes");
                    assert_eq!(doc.text().to_string(), "unsaved originalx\n");
                }),
            ),
            (Some(":qa!<ret>"), None),
        ],
        true,
    )
    .await?;
    assert_eq!(std::fs::read_to_string(&path)?, "disk change\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_review_requires_an_existing_chat_without_editing_the_document() -> anyhow::Result<()>
{
    let mut app = AppBuilder::new().with_input_text("#[hello|]#").build()?;
    test_key_sequence(
        &mut app,
        Some(":opencode-changes<ret>"),
        Some(&|app| {
            assert_eq!(doc!(app.editor).text().to_string(), "hello");
            assert!(app.editor.focused_pane.is_none());
            assert!(app
                .editor
                .get_status()
                .unwrap()
                .0
                .contains("Open a chat with :opencode first"));
        }),
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn space_o_closes_chat_while_opencode_keeps_it_open() -> anyhow::Result<()> {
    let mut app = AppBuilder::new().with_input_text("#[hello|]#").build()?;
    let original = std::cell::Cell::new(None);
    // Attach only: no session creation or inference is needed for pane routing.
    test_key_sequences(
        &mut app,
        vec![
            (
                Some(":opencode ses_toggle_test<ret>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                    original.set(app.editor.focused_pane);
                    assert_eq!(doc!(app.editor).text().to_string(), "hello");
                }),
            ),
            (
                Some(":opencode<ret>"),
                Some(&|app| assert!(app.editor.focused_pane.is_some())),
            ),
            (
                Some(" o"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_none());
                    assert_eq!(doc!(app.editor).text().to_string(), "hello");
                }),
            ),
            (
                Some(" o"),
                Some(&|app| {
                    assert_eq!(
                        app.editor.focused_pane,
                        original.get(),
                        "Reopening must restore the retained chat, not create a new session"
                    );
                }),
            ),
            (
                Some(":q<ret>"),
                Some(&|app| assert!(app.editor.focused_pane.is_none())),
            ),
            (
                Some(":opencode<ret>"),
                Some(&|app| assert_eq!(app.editor.focused_pane, original.get())),
            ),
            (Some(" o"), None),
            (
                Some(" o"),
                Some(&|app| assert_eq!(app.editor.focused_pane, original.get())),
            ),
            (
                Some("iKeep this draft<esc>:q<ret>"),
                Some(&|app| assert!(app.editor.focused_pane.is_none())),
            ),
            (
                Some(":opencode<ret>"),
                Some(&|app| {
                    assert_eq!(
                        app.editor.focused_pane,
                        original.get(),
                        "Reopen must restore the original chat, not create another connection"
                    );
                    assert_eq!(doc!(app.editor).text().to_string(), "hello");
                }),
            ),
            (Some(" o"), None),
        ],
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn selection_always_opens_comment_input_before_attaching_or_opening_chat(
) -> anyhow::Result<()> {
    for command in [
        ":opencode-selection<ret>",
        ":opencode-selection --comment 'Prefilled comment' Overall message<ret>",
    ] {
        let mut app = AppBuilder::new().with_input_text("#[hello|]#").build()?;
        test_key_sequences(
            &mut app,
            vec![
                (
                    Some(command),
                    Some(&|app| {
                        assert!(
                            app.editor.focused_pane.is_none(),
                            "Do not open or focus chat until the comment is confirmed"
                        );
                        assert_eq!(doc!(app.editor).text().to_string(), "hello");
                    }),
                ),
                (
                    Some(" additional comment"),
                    Some(&|app| {
                        assert!(app.editor.focused_pane.is_none());
                        assert_eq!(
                            doc!(app.editor).text().to_string(),
                            "hello",
                            "Typing a comment must not edit the selected document"
                        );
                    }),
                ),
                (
                    Some("<esc>:echo cancelled<ret>"),
                    Some(&|app| {
                        assert!(app.editor.focused_pane.is_none());
                        assert_eq!(doc!(app.editor).text().to_string(), "hello");
                        assert_eq!(app.editor.get_status().unwrap().0, "cancelled");
                    }),
                ),
            ],
            false,
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn commenting_requires_staged_context_without_editing_the_document() -> anyhow::Result<()> {
    let mut app = AppBuilder::new().with_input_text("#[hello|]#").build()?;
    test_key_sequence(
        &mut app,
        Some(":opencode-comment<ret>"),
        Some(&|app| {
            assert_eq!(doc!(app.editor).text().to_string(), "hello");
            assert!(app
                .editor
                .get_status()
                .unwrap()
                .0
                .contains("Open a chat with :opencode first"));
        }),
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn selection_attachments_capture_dirty_unicode_and_reversed_multiple_ranges(
) -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("a file.rs");
    std::fs::write(&path, "a😀b\nsecond\n")?;
    let mut app = AppBuilder::new().with_file(path.clone(), None).build()?;
    {
        let (view, doc) = helix_view::current!(app.editor);
        let transaction = helix_core::Transaction::change_by_selection(
            doc.text(),
            &helix_core::Selection::point(3),
            |_| (3, 3, Some("!".into())),
        );
        doc.apply(&transaction, view.id);
        doc.set_selection(
            view.id,
            helix_core::Selection::new(
                vec![helix_core::Range::new(4, 0), helix_core::Range::new(5, 11)].into(),
                0,
            ),
        );
    }
    let request = helix_term::ui::test_capture_opencode_selection(&app.editor)?;
    assert_eq!(request["files"].as_array().unwrap().len(), 2);
    assert_eq!(
        request["files"][0]["uri"],
        "data:text/plain;charset=utf-8,%61%F0%9F%98%80%62%21"
    );
    assert_eq!(request["metadata"]["comments"][0]["preview"], "a😀b!");
    assert_eq!(
        request["metadata"]["comments"][0]["selection"],
        serde_json::json!({
            "startLine":1,"endLine":1,"startChar":0,"endChar":5
        })
    );
    assert_eq!(request["metadata"]["comments"][1]["preview"], "second");
    assert_eq!(
        request["metadata"]["comments"][1]["selection"]["startLine"],
        2
    );
    let source = reqwest::Url::parse(request["files"][0]["source"]["uri"].as_str().unwrap())?;
    assert!(source.path().contains("a%20file.rs"));
    assert_eq!(source.query(), Some("start=1&end=1"));
    assert_eq!(doc!(app.editor).text().to_string(), "a😀b!\nsecond\n");
    assert!(doc!(app.editor).is_modified());
    assert_eq!(std::fs::read_to_string(&path)?, "a😀b\nsecond\n");
    {
        let (view, doc) = helix_view::current!(app.editor);
        doc.set_selection(view.id, helix_core::Selection::single(0, 5));
    }
    let request = helix_term::ui::test_capture_opencode_selection(&app.editor)?;
    assert_eq!(request["metadata"]["comments"][0]["preview"], "a😀b!\n");
    assert_eq!(
        request["metadata"]["comments"][0]["selection"]["endLine"],
        1
    );
    test_key_sequence(&mut app, Some(":q!<ret>"), None, true).await
}

#[tokio::test(flavor = "multi_thread")]
async fn unnamed_selections_are_inline_snapshots_without_invented_paths() -> anyhow::Result<()> {
    let mut app = AppBuilder::new().with_input_text("#[hello|]#").build()?;
    let request = helix_term::ui::test_capture_opencode_selection(&app.editor)?;
    assert_eq!(
        request["files"][0]["source"],
        serde_json::json!({"type":"inline"})
    );
    assert_eq!(request["metadata"]["comments"][0]["type"], "quote");
    assert_eq!(request["metadata"]["comments"][0]["text"], "hello");
    test_key_sequence(&mut app, Some(":q!<ret>"), None, true).await
}

#[tokio::test(flavor = "multi_thread")]
async fn model_picker_requires_chat_without_editing_the_document() -> anyhow::Result<()> {
    let mut app = AppBuilder::new().with_input_text("#[hello|]#").build()?;
    test_key_sequence(
        &mut app,
        Some(":opencode-model<ret>"),
        Some(&|app| {
            assert!(app.editor.focused_pane.is_none());
            assert_eq!(doc!(app.editor).text().to_string(), "hello");
            assert!(app
                .editor
                .get_status()
                .unwrap()
                .0
                .contains("Open a chat with :opencode first"));
        }),
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_session_ids_are_rejected_before_opening_a_connection() -> anyhow::Result<()> {
    let mut app = AppBuilder::new().with_input_text("#[hello|]#").build()?;
    test_key_sequence(
        &mut app,
        Some(":opencode ses_bad/path<ret>"),
        Some(&|app| {
            assert!(app.editor.focused_pane.is_none());
            assert_eq!(doc!(app.editor).text().to_string(), "hello");
            assert!(app
                .editor
                .get_status()
                .unwrap()
                .0
                .contains("Invalid OpenCode session ID"));
        }),
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn file_references_jump_in_dirty_buffers_without_reloading_or_writing() -> anyhow::Result<()>
{
    let root = tempfile::tempdir()?;
    let path = root.path().join("source.rs");
    std::fs::write(&path, "one\né界x\n")?;
    let mut app = AppBuilder::new().with_file(path.clone(), None).build()?;
    {
        let (view, doc) = helix_view::current!(app.editor);
        let transaction = helix_core::Transaction::change_by_selection(
            doc.text(),
            &helix_core::Selection::point(3),
            |_| (3, 3, Some("!".into())),
        );
        doc.apply(&transaction, view.id);
    }
    std::fs::write(&path, "different contents on disk\n")?;

    helix_term::ui::test_open_chat_reference(&mut app.editor, &path, Some(Position::new(1, 2)))?;
    let (view, doc) = helix_view::current_ref!(&app.editor);
    assert_eq!(doc.text().to_string(), "one!\né界x\n");
    assert!(doc.is_modified());
    assert_eq!(
        helix_core::coords_at_pos(
            doc.text().slice(..),
            doc.selection(view.id)
                .primary()
                .cursor(doc.text().slice(..))
        ),
        Position::new(1, 2)
    );
    assert_eq!(
        std::fs::read_to_string(&path)?,
        "different contents on disk\n"
    );

    helix_term::ui::test_open_chat_reference(
        &mut app.editor,
        &path,
        Some(Position::new(999, 999)),
    )?;
    let (view, doc) = helix_view::current_ref!(&app.editor);
    let head = doc.selection(view.id).primary().head;
    assert!(head <= doc.text().len_chars());
    helix_term::ui::test_open_chat_reference(&mut app.editor, &path, None)?;
    let (view, doc) = helix_view::current_ref!(&app.editor);
    assert_eq!(
        doc.selection(view.id).primary().head,
        head,
        "A path without coordinates preserves an existing cursor"
    );
    test_key_sequence(&mut app, Some(":q!<ret>"), None, true).await
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_base_and_inline_toggle_work_in_any_tracked_buffer() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let git = |args: &[&str]| -> anyhow::Result<()> {
        let output = std::process::Command::new("git")
            .current_dir(root.path())
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
        anyhow::ensure!(output.status.success(), "Fixture Git failed");
        Ok(())
    };
    git(&["init", "--quiet", "--initial-branch=main"])?;
    let path = root.path().join("source.rs");
    std::fs::write(&path, "base\n")?;
    git(&["add", "."])?;
    git(&["commit", "--quiet", "-m", "base"])?;
    git(&["checkout", "--quiet", "-b", "feature"])?;
    std::fs::write(&path, "feature\n")?;
    git(&["commit", "--quiet", "-am", "feature"])?;
    let mut app = AppBuilder::new().with_file(path.clone(), None).build()?;
    test_key_sequences(
        &mut app,
        vec![
            (
                Some(":toggle-inline-diff<ret>"),
                Some(&|app| {
                    let doc = doc!(app.editor);
                    assert!(doc.inline_diff);
                    assert_eq!(doc.diff_source(), None);
                }),
            ),
            (
                Some(":diff-base branch<ret>"),
                Some(&|app| {
                    let source = doc!(app.editor).diff_source().map(str::to_owned);
                    assert!(
                        source
                            .as_deref()
                            .is_some_and(|source| source.starts_with("main (")),
                        "{source:?}"
                    );
                    let base = doc!(app.editor)
                        .diff_handle()
                        .unwrap()
                        .load()
                        .diff_base()
                        .to_string();
                    assert_eq!(base, "base\n", "The branch point version, not HEAD");
                }),
            ),
            (
                Some(":diff-base head<ret>"),
                Some(&|app| {
                    assert_eq!(doc!(app.editor).diff_source(), None);
                    assert!(
                        doc!(app.editor).inline_diff,
                        "Changing the base keeps the display"
                    );
                }),
            ),
            (
                Some(":inline-diff<ret>"),
                Some(&|app| assert!(!doc!(app.editor).inline_diff)),
            ),
            (
                Some(":new<ret>:toggle-inline-diff<ret>"),
                Some(&|app| {
                    assert!(!doc!(app.editor).inline_diff);
                    assert!(app
                        .editor
                        .get_status()
                        .is_some_and(|(message, _)| message.contains("no diff base")));
                }),
            ),
        ],
        false,
    )
    .await
}
