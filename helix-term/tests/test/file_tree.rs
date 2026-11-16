use super::*;
use helix_view::{current_ref, doc};

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
