use super::*;
use helix_core::Position;
use helix_view::doc;

#[tokio::test(flavor = "multi_thread")]
async fn space_o_closes_chat_while_opencode_keeps_it_open() -> anyhow::Result<()> {
    let mut app = AppBuilder::new().with_input_text("#[hello|]#").build()?;
    // Attach only: no session creation or inference is needed for pane routing.
    test_key_sequences(
        &mut app,
        vec![
            (
                Some(":opencode ses_toggle_test<ret>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
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
