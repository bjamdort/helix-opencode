use super::*;
use helix_view::{current_ref, doc};

fn fixture() -> anyhow::Result<(tempfile::TempDir, helix_term::application::Application)> {
    let root = tempfile::tempdir()?;
    for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        std::fs::write(root.path().join(name), format!("{name}\n"))?;
    }
    let app = AppBuilder::new()
        .with_file(root.path().join("a.txt"), None)
        .build()?;
    Ok((root, app))
}

#[tokio::test(flavor = "multi_thread")]
async fn global_prompt_and_quit_all_work_with_pane_focus() -> anyhow::Result<()> {
    let (_root, mut app) = fixture()?;
    test_key_sequences(
        &mut app,
        vec![
            (
                Some(" .:echo hello<ret>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                    assert_eq!(app.editor.get_status().unwrap().0, "hello");
                    assert_eq!(doc!(app.editor).text().to_string(), "a.txt\n");
                }),
            ),
            (Some(":qa<ret>"), None),
        ],
        true,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn pane_focus_blocks_document_commands_and_repeat() -> anyhow::Result<()> {
    let (_root, mut app) = fixture()?;
    test_key_sequences(
        &mut app,
        vec![
            (
                Some("A!<esc> .i"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                    assert_eq!(app.editor.mode(), helix_view::document::Mode::Normal);
                    assert!(app
                        .editor
                        .get_status()
                        .unwrap()
                        .0
                        .contains("requires a document pane"));
                    assert_eq!(doc!(app.editor).text().to_string(), "a.txt!\n");
                }),
            ),
            (
                Some("du.p"),
                Some(&|app| {
                    assert_eq!(doc!(app.editor).text().to_string(), "a.txt!\n");
                    assert_eq!(app.editor.mode(), helix_view::document::Mode::Normal);
                }),
            ),
            (
                Some(":sort<ret>"),
                Some(&|app| {
                    assert!(app
                        .editor
                        .get_status()
                        .unwrap()
                        .0
                        .contains("requires a document pane"));
                    assert_eq!(doc!(app.editor).text().to_string(), "a.txt!\n");
                }),
            ),
        ],
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn remapped_commands_counts_and_sequences_work_in_panes() -> anyhow::Result<()> {
    use helix_term::{commands::MappableCommand, keymap::KeyTrie};
    use helix_view::document::Mode;
    let root = tempfile::tempdir()?;
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(root.path().join(name), name)?;
    }
    let mut config = Config::default();
    let keys = config
        .keys
        .get_mut(&Mode::Normal)
        .unwrap()
        .node_mut()
        .unwrap();
    keys.insert(
        "J".parse()?,
        KeyTrie::MappableCommand(MappableCommand::move_visual_line_down),
    );
    keys.insert(
        "Q".parse()?,
        KeyTrie::MappableCommand(MappableCommand::command_mode),
    );
    keys.insert(
        "Z".parse()?,
        KeyTrie::Sequence(vec![
            MappableCommand::pane_refresh,
            MappableCommand::delete_selection,
        ]),
    );
    keys.insert("Y".parse()?, KeyTrie::MappableCommand("@i".parse()?));
    keys.insert("X".parse()?, KeyTrie::MappableCommand(":sort".parse()?));
    keys.insert(
        "V".parse()?,
        KeyTrie::MappableCommand(":echo bound".parse()?),
    );
    keys.insert("W".parse()?, KeyTrie::MappableCommand(":q".parse()?));
    let mut app = AppBuilder::new()
        .with_file(root.path().join("a.txt"), None)
        .with_config(config)
        .build()?;
    test_key_sequences(
        &mut app,
        vec![
            (
                Some(" .Qecho remapped<ret>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                    assert_eq!(app.editor.get_status().unwrap().0, "remapped");
                }),
            ),
            (
                Some("ZYX"),
                Some(&|app| {
                    assert_eq!(doc!(app.editor).text().to_string(), "a.txt");
                    assert_eq!(app.editor.mode(), Mode::Normal);
                    assert!(app
                        .editor
                        .get_status()
                        .unwrap()
                        .0
                        .contains("requires a document pane"));
                }),
            ),
            (
                Some("V"),
                Some(&|app| {
                    assert_eq!(app.editor.get_status().unwrap().0, "bound");
                }),
            ),
            (
                Some("2J<ret>"),
                Some(&|app| {
                    assert_eq!(doc!(app.editor).path().unwrap(), &root.path().join("c.txt"));
                    assert!(app.editor.focused_pane.is_none());
                }),
            ),
            (
                Some("<C-w>hW"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_none());
                    assert_eq!(current_ref!(app.editor).0.area.x, 0);
                }),
            ),
        ],
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_cancellation_and_pending_keys_preserve_pane_focus() -> anyhow::Result<()> {
    let (root, mut app) = fixture()?;
    test_key_sequences(
        &mut app,
        vec![
            (
                Some(" .:<esc>g<esc>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                }),
            ),
            (
                Some("j<ret>"),
                Some(&|app| {
                    assert_eq!(doc!(app.editor).path().unwrap(), &root.path().join("b.txt"));
                    assert!(app.editor.focused_pane.is_none());
                }),
            ),
        ],
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn quit_closes_only_the_focused_pane() -> anyhow::Result<()> {
    let (_root, mut app) = fixture()?;
    test_key_sequences(
        &mut app,
        vec![
            (
                Some("A!<esc> .:q invalid<ret>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                }),
            ),
            (
                Some(":q<ret>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_none());
                    assert_eq!(app.editor.tree.views().count(), 1);
                    assert_eq!(current_ref!(app.editor).0.area.x, 0);
                    assert!(doc!(app.editor).is_modified());
                }),
            ),
        ],
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn quit_all_still_protects_unsaved_documents_from_a_pane() -> anyhow::Result<()> {
    let (_root, mut app) = fixture()?;
    test_key_sequences(
        &mut app,
        vec![
            (
                Some("A!<esc> .:qa<ret>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                    assert!(doc!(app.editor).is_modified());
                    assert!(app.editor.get_status().is_some());
                }),
            ),
            (Some(":qa!<ret>"), None),
        ],
        true,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn window_navigation_includes_panes_and_document_splits() -> anyhow::Result<()> {
    let (_root, mut app) = fixture()?;
    test_key_sequences(
        &mut app,
        vec![
            (
                Some(":vsplit<ret> .<C-w>l"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_none());
                    assert_eq!(app.editor.tree.views().count(), 2);
                }),
            ),
            (
                Some("<C-w>l<C-w>h<C-w>h"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                }),
            ),
            (
                Some("<C-w>w"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_none());
                }),
            ),
            (
                Some("<C-w>w<C-w>w"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                }),
            ),
            (
                Some("<C-w>q"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_none());
                    assert_eq!(app.editor.tree.views().count(), 2);
                }),
            ),
            (Some(":qa<ret>"), None),
        ],
        true,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn command_palette_executes_pane_actions_and_global_prompts() -> anyhow::Result<()> {
    let (_root, mut app) = fixture()?;
    test_key_sequences(
        &mut app,
        vec![
            (
                Some(" . ?pane_refresh<ret>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                }),
            ),
            (Some(" ?command_mode<ret>"), None),
            (
                Some("echo palette<ret>"),
                Some(&|app| {
                    assert!(app.editor.focused_pane.is_some());
                    assert_eq!(app.editor.get_status().unwrap().0, "palette");
                }),
            ),
        ],
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn sidebar_opens_files_and_returns_focus_to_editor() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let initial = root.path().join("initial.txt");
    let child = root.path().join("directory/child.txt");
    std::fs::create_dir(root.path().join("directory"))?;
    std::fs::write(&initial, "initial\n")?;
    std::fs::write(&child, "child\n")?;
    let mut app = AppBuilder::new().with_file(&initial, None).build()?;

    test_key_sequences(
        &mut app,
        vec![
            (
                Some(" .lj<ret>"),
                Some(&|app| {
                    assert_eq!(doc!(app.editor).path().unwrap(), &child);
                    assert_eq!(doc!(app.editor).text().to_string(), "child\n");
                    let (view, _) = current_ref!(app.editor);
                    assert!(view.area.x > 0);
                }),
            ),
            (
                Some("<C-w>hi<C-w>lA!<esc>"),
                Some(&|app| {
                    assert_eq!(doc!(app.editor).text().to_string(), "child!\n");
                }),
            ),
            (
                Some("<C-w>hq"),
                Some(&|app| {
                    let (view, _) = current_ref!(app.editor);
                    assert_eq!(view.area.x, 0);
                }),
            ),
        ],
        false,
    )
    .await
}
