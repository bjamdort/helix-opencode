//! Read-only change review shared by the filtered explorer and a central pane.
mod git;

use super::{
    file_tree::FileTree,
    pane::{Dock, Pane, PaneOutcome},
};
use crate::{
    commands::pane::PaneCommand,
    compositor,
    job::{Callback, Jobs},
    keymap::{macros::keymap, KeyTrie},
    opencode::changes::{ChangesHandle, Turn},
};
use anyhow::{Context as _, Result};
use helix_core::{
    hashmap,
    unicode::{segmentation::UnicodeSegmentation, width::UnicodeWidthStr},
    Position,
};
use helix_view::{
    graphics::{CursorKind, Modifier, Rect, Style},
    input::{MouseEvent, MouseEventKind},
    DocumentId, Editor, PaneId, Theme,
};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, HashSet},
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use tui::{
    buffer::Buffer,
    text::{Span, Spans},
};

pub(crate) use git::{branch_point, file_at};

pub(super) const ID: &str = "changes";
pub(super) type SharedReview = Arc<Mutex<Review>>;

#[derive(Clone, Debug)]
pub(super) struct ChangeFile {
    path: PathBuf,
    original: Option<PathBuf>,
    status: String,
    patch: Option<String>,
    additions: u64,
    deletions: u64,
}

impl ChangeFile {
    fn marker(&self) -> &str {
        match self.status.as_str() {
            "added" => "A",
            "deleted" => "D",
            "renamed" => "R",
            "untracked" => "?",
            "conflict" => "U",
            _ => "M",
        }
    }
    fn scope(&self) -> &str {
        match self.status.as_str() {
            "added" | "untracked" => "diff.plus",
            "deleted" => "diff.minus",
            "conflict" => "diff.delta.conflict",
            _ => "diff.delta",
        }
    }
}

#[derive(Clone)]
enum Source {
    Git,
    Branch(git::BranchPoint),
    Turn { handle: ChangesHandle, turn: Turn },
}

impl Source {
    fn uses_git(&self) -> bool {
        matches!(self, Self::Git | Self::Branch(_))
    }
}

pub(super) struct Review {
    root: PathBuf,
    title: String,
    source: Source,
    files: Vec<ChangeFile>,
    /// The file shown in the diff pane.
    selected: usize,
    /// The highlighted row of the folder tree, which can be a directory.
    cursor: usize,
    collapsed: HashSet<PathBuf>,
    list_offset: usize,
    /// Buffers opened from this review, restored to `HEAD` without inline
    /// changes when review ends.
    opened: Vec<DocumentId>,
    /// Whether the read-only diff pane needs patches for the selected file.
    want_patch: bool,
    requested: bool,
    loading: bool,
    error: Option<String>,
    patch_pending: Option<PathBuf>,
    patch_error: Option<(PathBuf, String)>,
    generation: u64,
}

impl Review {
    fn new(root: PathBuf, title: String, source: Source) -> SharedReview {
        Arc::new(Mutex::new(Self {
            root,
            title,
            source,
            files: Vec::new(),
            selected: 0,
            cursor: 0,
            collapsed: HashSet::new(),
            list_offset: 0,
            opened: Vec::new(),
            want_patch: false,
            requested: true,
            loading: false,
            error: None,
            patch_pending: None,
            patch_error: None,
            generation: 0,
        }))
    }

    /// Changed files grouped into folders, directories first, like the explorer.
    fn rows(&self) -> Vec<ListRow> {
        #[derive(Default)]
        struct Node {
            directories: BTreeMap<String, Node>,
            files: Vec<(String, usize)>,
        }
        fn walk(
            node: &Node,
            prefix: &Path,
            depth: usize,
            collapsed: &HashSet<PathBuf>,
            rows: &mut Vec<ListRow>,
        ) {
            for (name, child) in &node.directories {
                let path = prefix.join(name);
                let expanded = !collapsed.contains(&path);
                rows.push(ListRow {
                    path: path.clone(),
                    name: name.clone(),
                    depth,
                    file: None,
                    expanded,
                });
                if expanded {
                    walk(child, &path, depth + 1, collapsed, rows);
                }
            }
            let mut files = node.files.clone();
            files.sort();
            for (name, index) in files {
                rows.push(ListRow {
                    path: prefix.join(&name),
                    name,
                    depth,
                    file: Some(index),
                    expanded: false,
                });
            }
        }
        let mut root = Node::default();
        for (index, file) in self.files.iter().enumerate() {
            let parts: Vec<String> = file
                .path
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            let Some((name, directories)) = parts.split_last() else {
                continue;
            };
            let mut node = &mut root;
            for directory in directories {
                node = node.directories.entry(directory.clone()).or_default();
            }
            node.files.push((name.clone(), index));
        }
        let mut rows = Vec::new();
        walk(&root, Path::new(""), 0, &self.collapsed, &mut rows);
        rows
    }

    /// Point the tree cursor at the selected file, revealing its folders.
    fn reveal_selected(&mut self) {
        if let Some(file) = self.files.get(self.selected) {
            for parent in file.path.ancestors().skip(1) {
                self.collapsed.remove(parent);
            }
        }
        let selected = self.selected;
        self.cursor = self
            .rows()
            .iter()
            .position(|row| row.file == Some(selected))
            .unwrap_or(0);
    }

    fn move_cursor(&mut self, rows: &[ListRow], cursor: usize) {
        self.cursor = cursor.min(rows.len().saturating_sub(1));
        if let Some(index) = rows.get(self.cursor).and_then(|row| row.file) {
            self.selected = index;
        }
    }
}

struct ListRow {
    path: PathBuf,
    name: String,
    depth: usize,
    file: Option<usize>,
    expanded: bool,
}

fn trusted(editor: &Editor, root: &Path) -> bool {
    editor
        .workspace_trust
        .query(
            &helix_loader::find_workspace_in(root).0,
            helix_loader::workspace_trust::TrustQuery::Git,
        )
        .is_trusted()
}

fn validate_relative(path: &Path) -> Result<()> {
    anyhow::ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "Changed file path must stay inside the review workspace"
    );
    Ok(())
}

/// Snapshot diffs use repository-relative paths, even for a session in a
/// subdirectory. Resolve that prefix locally without invoking Git.
fn turn_file_path(root: &Path, path: PathBuf) -> Result<PathBuf> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    if path.is_absolute() {
        let path = path
            .strip_prefix(&root)
            .context("Changed file is outside the turn workspace")?
            .to_owned();
        validate_relative(&path)?;
        return Ok(path);
    }
    validate_relative(&path)?;
    let worktree = root
        .ancestors()
        .find(|parent| parent.join(".git").exists())
        .unwrap_or(&root);
    let scope = root.strip_prefix(worktree)?;
    let path = path
        .strip_prefix(scope)
        .context("Changed file is outside the turn workspace")?
        .to_owned();
    validate_relative(&path)?;
    Ok(path)
}

pub(super) fn update(shared: &SharedReview, editor: &Editor, jobs: &mut Jobs) {
    let (root, source, generation, load, file) = {
        let mut review = shared.lock();
        if review.requested {
            review.requested = false;
            review.generation += 1;
            review.loading = true;
            review.error = None;
            review.patch_pending = None;
            review.patch_error = None;
            (
                review.root.clone(),
                review.source.clone(),
                review.generation,
                true,
                None,
            )
        } else {
            if review.loading || review.patch_pending.is_some() || !review.want_patch {
                return;
            }
            let Some(file) = review.files.get(review.selected) else {
                return;
            };
            if file.patch.is_some()
                || review
                    .patch_error
                    .as_ref()
                    .is_some_and(|(path, _)| path == &file.path)
            {
                return;
            }
            let file = file.clone();
            review.patch_pending = Some(file.path.clone());
            (
                review.root.clone(),
                review.source.clone(),
                review.generation,
                false,
                Some(file),
            )
        }
    };
    if source.uses_git() && !trusted(editor, &root) {
        let mut review = shared.lock();
        review.files.clear();
        review.loading = false;
        review.patch_pending = None;
        review.error = Some("Trust this workspace before running Git review".into());
        return;
    }
    let shared = shared.clone();
    jobs.callback(async move {
        let result = if load {
            load_files(&root, &source).await
        } else {
            let mut file = file.unwrap();
            let base = match &source {
                Source::Branch(point) => Some(point.commit.as_str()),
                _ => None,
            };
            match git::patch(&root, &file, base).await {
                Ok(patch) => {
                    file.additions = patch
                        .lines()
                        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
                        .count() as u64;
                    file.deletions = patch
                        .lines()
                        .filter(|line| line.starts_with('-') && !line.starts_with("---"))
                        .count() as u64;
                    file.patch = Some(patch);
                    Ok(vec![file])
                }
                Err(error) => Err(error),
            }
        };
        Ok(Callback::EditorCompositor(Box::new(move |editor, _| {
            let mut review = shared.lock();
            if review.generation != generation {
                return;
            }
            if source.uses_git() && !trusted(editor, &root) {
                review.files.clear();
                review.loading = false;
                review.patch_pending = None;
                review.error =
                    Some("Workspace trust was revoked; Git review is unavailable".into());
                return;
            }
            if load {
                let selected = review
                    .files
                    .get(review.selected)
                    .map(|file| file.path.clone());
                review.loading = false;
                match result {
                    Ok(mut files) => {
                        files.sort_by(|a, b| a.path.cmp(&b.path));
                        review.selected = selected
                            .and_then(|path| files.iter().position(|file| file.path == path))
                            .unwrap_or(0);
                        review.files = files;
                        review.reveal_selected();
                    }
                    Err(error) => {
                        review.files.clear();
                        review.error = Some(format!("{error:#}"));
                    }
                }
            } else {
                let path = review.patch_pending.take();
                match result {
                    Ok(mut files) => {
                        if let Some(file) = files.pop() {
                            if let Some(slot) =
                                review.files.iter_mut().find(|slot| slot.path == file.path)
                            {
                                *slot = file;
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(path) = path {
                            review.patch_error = Some((path, format!("{error:#}")));
                        }
                    }
                }
            }
            helix_event::request_redraw();
        })))
    });
}

async fn load_files(root: &Path, source: &Source) -> Result<Vec<ChangeFile>> {
    match source {
        Source::Git => git::files(root).await,
        Source::Branch(point) => git::branch_files(root, &point.commit).await,
        Source::Turn { handle, turn } => {
            let files = handle
                .diff(turn.id.clone())?
                .await
                .context("OpenCode connection changed during turn review")??;
            anyhow::ensure!(
                files.len() <= 10_000
                    && files.iter().map(|file| file.patch.len()).sum::<usize>() <= 32 * 1024 * 1024,
                "Turn diff exceeds the review size limit"
            );
            files
                .into_iter()
                .map(|file| {
                    let path = turn_file_path(root, PathBuf::from(file.file))?;
                    anyhow::ensure!(
                        file.patch.len() <= 2 * 1024 * 1024,
                        "File diff exceeds the 2 MiB review limit"
                    );
                    Ok(ChangeFile {
                        path,
                        original: None,
                        status: file.status,
                        patch: Some(file.patch),
                        additions: file.additions,
                        deletions: file.deletions,
                    })
                })
                .collect()
        }
    }
}

pub(super) fn list_command(
    shared: &SharedReview,
    command: PaneCommand,
    count: usize,
    height: u16,
) -> Result<PaneOutcome> {
    let mut review = shared.lock();
    let rows = review.rows();
    let cursor = review.cursor;
    let page = (height.saturating_sub(2) as usize).saturating_mul(count);
    let row = rows.get(cursor);
    let show = |shared: &SharedReview| {
        let shared = shared.clone();
        Ok(PaneOutcome::Callback(Box::new(move |compositor, cx| {
            open_selected(compositor, cx, shared)
        })))
    };
    match command {
        PaneCommand::Up => review.move_cursor(&rows, cursor.saturating_sub(count)),
        PaneCommand::Down => review.move_cursor(&rows, cursor.saturating_add(count)),
        PaneCommand::First => review.move_cursor(&rows, count.saturating_sub(1)),
        PaneCommand::Last => review.move_cursor(&rows, usize::MAX),
        PaneCommand::PageUp => review.move_cursor(&rows, cursor.saturating_sub(page)),
        PaneCommand::PageDown => review.move_cursor(&rows, cursor.saturating_add(page)),
        PaneCommand::Activate | PaneCommand::Right => match row {
            Some(row) if row.file.is_none() => {
                if row.expanded {
                    if matches!(command, PaneCommand::Activate) {
                        review.collapsed.insert(row.path.clone());
                    }
                } else {
                    review.collapsed.remove(&row.path);
                }
            }
            _ => return show(shared),
        },
        PaneCommand::Left => match row {
            Some(row) if row.file.is_none() && row.expanded => {
                review.collapsed.insert(row.path.clone());
            }
            Some(row) => {
                let depth = row.depth;
                if let Some(parent) = rows[..cursor].iter().rposition(|row| row.depth < depth) {
                    review.cursor = parent;
                }
            }
            None => (),
        },
        PaneCommand::Refresh => review.requested = true,
        PaneCommand::Close => return Ok(PaneOutcome::Callback(close_callback())),
        PaneCommand::FocusDocument => return Ok(PaneOutcome::FocusDocument),
        _ => anyhow::bail!("This action is not supported in the changes explorer"),
    }
    Ok(PaneOutcome::None)
}

pub(super) fn list_mouse(shared: &SharedReview, mouse: &MouseEvent, area: Rect) {
    let mut review = shared.lock();
    let rows = review.rows();
    let cursor = review.cursor;
    match mouse.kind {
        MouseEventKind::Down(helix_view::input::MouseButton::Left)
            if mouse.row > area.y && mouse.row < area.bottom().saturating_sub(1) =>
        {
            let row = review.list_offset + (mouse.row - area.y - 1) as usize;
            if row < rows.len() {
                review.move_cursor(&rows, row);
                if rows[row].file.is_none() {
                    let path = rows[row].path.clone();
                    if !review.collapsed.remove(&path) {
                        review.collapsed.insert(path);
                    }
                }
            }
        }
        MouseEventKind::ScrollUp => review.move_cursor(&rows, cursor.saturating_sub(3)),
        MouseEventKind::ScrollDown => review.move_cursor(&rows, cursor + 3),
        _ => (),
    }
}

fn write(surface: &mut Buffer, area: Rect, text: &str, style: Style) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    surface.set_stringn(area.x, area.y, &text, area.width as usize, style);
}

pub(super) fn render_list(
    shared: &SharedReview,
    area: Rect,
    surface: &mut Buffer,
    theme: &Theme,
    focused: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut review = shared.lock();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            surface.set_string(x, y, " ", theme.get("ui.background"));
        }
    }
    let width = area.width;
    write(
        surface,
        area.with_width(width).with_height(1),
        &review.title,
        theme.get("ui.statusline"),
    );
    let height = area.height.saturating_sub(2) as usize;
    let rows = review.rows();
    review.cursor = review.cursor.min(rows.len().saturating_sub(1));
    if review.cursor < review.list_offset {
        review.list_offset = review.cursor;
    }
    if review.cursor >= review.list_offset + height {
        review.list_offset = review.cursor.saturating_sub(height.saturating_sub(1));
    }
    let message = if review.loading {
        Some("Loading changes…")
    } else if let Some(error) = &review.error {
        Some(error.as_str())
    } else if review.files.is_empty() {
        Some("No changed files")
    } else {
        None
    };
    if let Some(message) = message {
        if area.height > 2 {
            write(
                surface,
                Rect::new(area.x, area.y + 1, width, 1),
                message,
                theme.get("ui.text.inactive"),
            );
        }
    } else {
        for (index, row) in rows
            .iter()
            .enumerate()
            .skip(review.list_offset)
            .take(height)
        {
            let y = area.y + (index - review.list_offset) as u16 + 1;
            let file = row.file.and_then(|index| review.files.get(index));
            let mut style = if file.is_some() {
                theme.get("ui.text")
            } else {
                theme.get("ui.text.directory")
            };
            if index == review.cursor {
                // The selection stays visible while the diff pane has focus.
                style = style.patch(theme.get("ui.selection"));
                if focused {
                    style = style.patch(theme.get("ui.text.focus"));
                }
            }
            if let Some(color) = file.and_then(|file| theme.get(file.scope()).fg) {
                style = style.fg(color);
            }
            surface.set_style(Rect::new(area.x, y, width, 1), style);
            let label = format!(
                "{}{} {}",
                "  ".repeat(row.depth),
                super::file_tree::icon(&row.path, file.is_none(), row.expanded),
                row.name
            );
            write(
                surface,
                Rect::new(area.x, y, width.saturating_sub(2), 1),
                &label,
                style,
            );
            if let (Some(file), true) = (file, width >= 2) {
                write(
                    surface,
                    Rect::new(area.x + width - 1, y, 1, 1),
                    file.marker(),
                    style,
                );
            }
        }
    }
    if area.height > 1 {
        write(
            surface,
            Rect::new(area.x, area.bottom() - 1, width, 1),
            "Enter open/fold  r refresh  q close",
            theme.get("ui.text.inactive"),
        );
    }
}

/// The read-only diff pane, for changes that have no buffer to open, such as
/// deleted files.
fn show_diff(
    compositor: &mut compositor::Compositor,
    cx: &mut compositor::Context,
    shared: SharedReview,
) {
    shared.lock().want_patch = true;
    if let Some(view) = compositor.find::<super::EditorView>() {
        if let Some(id) = view.panes.find(ID) {
            *view.panes.get_mut::<DiffPane>(id).unwrap() = DiffPane::new(shared);
            view.panes.focus_pane(id, cx.editor);
        } else {
            view.panes
                .insert(DiffPane::new(shared), Dock::Center, cx.editor);
        }
    }
}

/// The file before a turn, from a whole-file patch: context and removed lines.
/// `None` when the patch is not a single whole-file hunk, such as a binary file.
fn turn_base(patch: &str) -> Option<Vec<u8>> {
    let headers: Vec<_> = patch
        .lines()
        .filter(|line| line.starts_with("@@"))
        .collect();
    let [header] = headers.as_slice() else {
        return None;
    };
    let old_start = header
        .split_whitespace()
        .nth(1)?
        .strip_prefix('-')?
        .split(',')
        .next()?;
    if !matches!(old_start, "0" | "1") {
        return None;
    }
    let mut base = String::new();
    let mut started = false;
    let mut last_old = false;
    for line in patch.split('\n') {
        if line.starts_with("@@") {
            started = true;
            continue;
        }
        if !started {
            continue;
        }
        match line.as_bytes().first() {
            Some(b' ' | b'-') => {
                base.push_str(&line[1..]);
                base.push('\n');
                last_old = true;
            }
            Some(b'+') => last_old = false,
            // "\ No newline at end of file" after an old line.
            Some(b'\\') if last_old => {
                base.pop();
                last_old = false;
            }
            _ => (),
        }
    }
    Some(base.into_bytes())
}

/// Open the selected change as a buffer that shows its changes inline.
fn open_selected(
    compositor: &mut compositor::Compositor,
    cx: &mut compositor::Context,
    shared: SharedReview,
) {
    let (root, source, file) = {
        let review = shared.lock();
        let Some(file) = review.files.get(review.selected).cloned() else {
            return;
        };
        (review.root.clone(), review.source.clone(), file)
    };
    let path = match (super::opencode::references::FileTarget {
        path: file.path.clone(),
        line: None,
        column: None,
    })
    .resolve(&root)
    {
        Ok(path) if file.status != "deleted" => path,
        _ => return show_diff(compositor, cx, shared),
    };
    match source {
        Source::Git => finish_open(compositor, cx.editor, &shared, &path, None),
        Source::Turn { turn, .. } => match file.patch.as_deref().and_then(turn_base) {
            Some(base) => {
                let label = format!("before turn {}", turn.label);
                finish_open(compositor, cx.editor, &shared, &path, Some((base, label)))
            }
            None => show_diff(compositor, cx, shared),
        },
        Source::Branch(point) => {
            if !trusted(cx.editor, &root) {
                cx.editor
                    .set_error("Trust this workspace before running Git review");
                return;
            }
            let original = file.original.clone().unwrap_or(file.path.clone());
            cx.jobs.callback(async move {
                let base = git::file_at(&root, &point.commit, &original).await;
                Ok(Callback::EditorCompositor(Box::new(
                    move |editor, compositor| match base {
                        Ok(base) => finish_open(
                            compositor,
                            editor,
                            &shared,
                            &path,
                            Some((base, point.label)),
                        ),
                        Err(error) => editor.set_error(format!("{error:#}")),
                    },
                )))
            });
        }
    }
}

fn finish_open(
    compositor: &mut compositor::Compositor,
    editor: &mut Editor,
    shared: &SharedReview,
    path: &Path,
    base: Option<(Vec<u8>, String)>,
) {
    let id = match editor.open(path, helix_view::editor::Action::Replace) {
        Ok(id) => id,
        Err(error) => return editor.set_error(error.to_string()),
    };
    let label = match base {
        Some((base, label)) => {
            if let Some(doc) = editor.documents.get_mut(&id) {
                doc.set_custom_diff_base(base, label.clone());
            }
            label
        }
        None => {
            editor.reset_diff_base(id);
            // Untracked files have no `HEAD` version: every line is new.
            if let Some(doc) = editor
                .documents
                .get_mut(&id)
                .filter(|doc| doc.diff_handle().is_none())
            {
                doc.set_custom_diff_base(Vec::new(), "HEAD".into());
            }
            "HEAD".into()
        }
    };
    if let Some(doc) = editor.documents.get_mut(&id) {
        doc.inline_diff = true;
    }
    {
        let mut review = shared.lock();
        review.want_patch = false;
        if !review.opened.contains(&id) {
            review.opened.push(id);
        }
    }
    if let Some(view) = compositor.find::<super::EditorView>() {
        if let Some(pane) = view.panes.find(ID) {
            view.panes.remove(pane, editor);
        }
        view.panes.focus_document(editor);
    }
    editor.set_status(format!("Showing changes against {label}"));
}

/// Turn off inline changes in buffers opened by a review and compare them with
/// `HEAD` again. Edits stay.
pub(super) fn restore(editor: &mut Editor, review: &SharedReview) {
    let opened = std::mem::take(&mut review.lock().opened);
    for id in opened {
        if let Some(doc) = editor.documents.get_mut(&id) {
            doc.inline_diff = false;
            editor.reset_diff_base(id);
        }
    }
}

fn install(
    compositor: &mut compositor::Compositor,
    editor: &mut Editor,
    shared: SharedReview,
) -> Result<()> {
    let root = shared.lock().root.clone();
    let view = compositor
        .find::<super::EditorView>()
        .context("Editor view is unavailable")?;
    if let Some(id) = view.panes.find(ID) {
        view.panes.remove(id, editor);
    }
    let tree_id = if let Some(id) = view.panes.find(super::file_tree::ID) {
        id
    } else {
        view.panes
            .insert(FileTree::new(root)?, Dock::Workspace, editor)
    };
    let tree = view.panes.get_mut::<FileTree>(tree_id).unwrap();
    if let Some(previous) = tree.changes.replace(shared) {
        restore(editor, &previous);
    }
    view.panes.focus_pane(tree_id, editor);
    Ok(())
}

/// The directory to review: the chat's session, the explorer's root, or the
/// current document's workspace.
fn review_root(
    compositor: &mut compositor::Compositor,
    editor: &Editor,
    document_root: PathBuf,
) -> PathBuf {
    compositor
        .find::<super::EditorView>()
        .and_then(|view| {
            let id = editor.focused_pane?;
            if let Some(chat) = view.panes.get_mut::<super::opencode::OpenCode>(id) {
                return Some(chat.session_directory());
            }
            if let Some(tree) = view.panes.get_mut::<FileTree>(id) {
                return Some(
                    tree.changes
                        .as_ref()
                        .map_or_else(|| tree.root.clone(), |review| review.lock().root.clone()),
                );
            }
            view.panes
                .get_mut::<DiffPane>(id)
                .map(|pane| pane.shared.lock().root.clone())
        })
        .unwrap_or(document_root)
}

pub(crate) fn git_callback(editor: &Editor) -> compositor::Callback {
    let document_root = helix_view::doc!(editor).workspace_root().to_owned();
    Box::new(move |compositor, cx| {
        let root = review_root(compositor, cx.editor, document_root);
        let review = Review::new(
            root,
            "Git changes · HEAD → working tree".into(),
            Source::Git,
        );
        if let Err(error) = install(compositor, cx.editor, review) {
            cx.editor.set_error(error.to_string());
        }
    })
}

/// Review everything that changed since the current branch split from the
/// default branch, including uncommitted and untracked files.
pub(crate) fn branch_callback(editor: &Editor) -> compositor::Callback {
    let document_root = helix_view::doc!(editor).workspace_root().to_owned();
    Box::new(move |compositor, cx| {
        let root = review_root(compositor, cx.editor, document_root);
        if !trusted(cx.editor, &root) {
            cx.editor
                .set_error("Trust this workspace before running Git review");
            return;
        }
        cx.jobs.callback(async move {
            let point = git::branch_point(&root).await;
            Ok(Callback::EditorCompositor(Box::new(
                move |editor, compositor| {
                    let point = match point {
                        Ok(point) => point,
                        Err(error) => return editor.set_error(format!("{error:#}")),
                    };
                    let title = format!("Branch changes · since {}", point.label);
                    let review = Review::new(root, title, Source::Branch(point));
                    if let Err(error) = install(compositor, editor, review) {
                        editor.set_error(error.to_string());
                    }
                },
            )))
        });
    })
}

pub(crate) fn close_callback() -> compositor::Callback {
    Box::new(|compositor, cx| {
        if let Some(view) = compositor.find::<super::EditorView>() {
            if let Some(id) = view.panes.find(ID) {
                view.panes.remove(id, cx.editor);
            }
            if let Some(id) = view.panes.find(super::file_tree::ID) {
                if let Some(review) = view.panes.get_mut::<FileTree>(id).unwrap().changes.take() {
                    restore(cx.editor, &review);
                }
                view.panes.focus_pane(id, cx.editor);
            }
        }
    })
}

pub(crate) fn opencode_callback(from: Option<String>) -> compositor::Callback {
    Box::new(move |compositor, cx| {
        let result = (|| -> Result<_> {
            let view = compositor
                .find::<super::EditorView>()
                .context("Editor view is unavailable")?;
            let id = view
                .panes
                .find(super::opencode::ID)
                .context("Open a chat with :opencode first")?;
            let handle = view
                .panes
                .get_mut::<super::opencode::OpenCode>(id)
                .unwrap()
                .changes_handle()?;
            let turns = handle.turns();
            anyhow::ensure!(
                !turns.is_empty(),
                "No user turns in the loaded chat history"
            );
            Ok((id, handle, turns))
        })();
        let (id, handle, turns) = match result {
            Ok(value) => value,
            Err(error) => {
                cx.editor.set_error(error.to_string());
                return;
            }
        };
        let select = move |turn: Turn| {
            let handle = handle.clone();
            Box::new(
                move |compositor: &mut compositor::Compositor, cx: &mut compositor::Context| {
                    let valid = compositor
                        .find::<super::EditorView>()
                        .and_then(|view| view.panes.get_mut::<super::opencode::OpenCode>(id))
                        .is_some_and(|pane| pane.matches_changes_handle(&handle));
                    if !valid {
                        cx.editor
                            .set_error("Chat changed; reopen :opencode-changes");
                        return;
                    }
                    let title = format!("Turn · {}", turn.label);
                    let review =
                        Review::new(turn.directory.clone(), title, Source::Turn { handle, turn });
                    if let Err(error) = install(compositor, cx.editor, review) {
                        cx.editor.set_error(error.to_string());
                    }
                },
            ) as compositor::Callback
        };
        if let Some(from) = from {
            if let Some(turn) = turns.iter().find(|turn| turn.id == from) {
                select(turn.clone())(compositor, cx);
            } else {
                cx.editor
                    .set_error("User message is not a turn anchor in the loaded chat history");
            }
        } else {
            // Turns are listed oldest first; start on the latest.
            let latest = turns.len().saturating_sub(1) as u32;
            let picker = super::Picker::new(
                [
                    super::PickerColumn::new("turn", |turn: &Turn, _| turn.label.as_str().into()),
                    super::PickerColumn::new("message", |turn: &Turn, _| turn.id.as_str().into()),
                ],
                0,
                turns,
                (),
                |_, _, _| {},
            )
            .with_initial_cursor(latest)
            .with_compositor_callback(move |_, turn, _| Some(select(turn.clone())));
            compositor.push(Box::new(super::overlay::overlaid(picker)));
        }
    })
}

struct DiffPane {
    shared: SharedReview,
    keys: KeyTrie,
    offset: usize,
    rows: usize,
    height: usize,
    path: Option<PathBuf>,
    cache: Option<Rendered>,
}

/// Highlighting a whole file is expensive, so it is done once per patch and theme.
struct Rendered {
    path: PathBuf,
    patch: String,
    theme: String,
    lines: Vec<DiffLine>,
    width: u16,
    rows: Vec<Spans<'static>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineKind {
    Context,
    Added,
    Removed,
    /// A gap between hunks, when the patch omits unchanged lines.
    Hunk,
    /// Metadata such as renames, mode changes, and binary files.
    Note,
}

struct DiffLine {
    kind: LineKind,
    old: Option<usize>,
    new: Option<usize>,
    spans: Vec<Span<'static>>,
}

struct DiffColors {
    added: Style,
    removed: Style,
    plus: Style,
    minus: Style,
    gutter: Style,
    hunk: Style,
}

impl DiffColors {
    fn new(theme: &Theme) -> Self {
        let added = super::text_decorations::diff::added_background(theme);
        let removed = super::text_decorations::diff::removed_background(theme);
        let hunk = theme.get("ui.text.inactive").patch(
            theme
                .get("ui.popup")
                .bg
                .map_or_else(Style::default, |color| Style::default().bg(color)),
        );
        Self {
            added: Style::default().bg(added),
            removed: Style::default().bg(removed),
            plus: theme.get("diff.plus"),
            minus: theme.get("diff.minus"),
            gutter: theme.get("ui.linenr"),
            hunk,
        }
    }

    fn row(&self, kind: LineKind) -> Style {
        match kind {
            LineKind::Added => self.added,
            LineKind::Removed => self.removed,
            LineKind::Hunk => self.hunk,
            LineKind::Context | LineKind::Note => Style::default(),
        }
    }
}

/// Expand tabs and drop control characters, including whole CSI escape
/// sequences, so file content cannot restyle the terminal.
fn clean(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\t' => output.push_str("    "),
            '\u{1b}' if chars.next_if_eq(&'[').is_some() => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            c if c.is_control() => (),
            c => output.push(c),
        }
    }
    output
}

/// Parse a unified patch into display lines. Old and new file text are
/// highlighted separately so each side keeps valid syntax.
fn diff_lines(
    patch: &str,
    path: &Path,
    theme: &Theme,
    loader: &helix_core::syntax::Loader,
) -> Vec<DiffLine> {
    let text = theme.get("ui.text");
    let muted = theme.get("ui.text.inactive");
    if !patch.lines().any(|line| line.starts_with("@@")) {
        return patch
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| DiffLine {
                kind: LineKind::Note,
                old: None,
                new: None,
                spans: vec![Span::styled(clean(line), muted)],
            })
            .collect();
    }
    let mut lines = Vec::new();
    let (mut old, mut new) = (0usize, 0usize);
    let mut started = false;
    let mut old_text = String::new();
    let mut new_text = String::new();
    for line in patch.lines() {
        if let Some(header) = line.strip_prefix("@@") {
            let mut ranges = header.split_whitespace();
            let start = |range: Option<&str>| {
                range
                    .and_then(|range| range.get(1..))
                    .and_then(|range| range.split(',').next())
                    .and_then(|number| number.parse::<usize>().ok())
                    .unwrap_or(1)
            };
            let old_start = start(ranges.next());
            let new_start = start(ranges.next());
            if started || new_start > 1 || old_start > 1 {
                let context = header
                    .split_once("@@")
                    .map_or("", |(_, context)| context.trim());
                let label = if context.is_empty() {
                    format!("⋯ line {new_start}")
                } else {
                    format!("⋯ line {new_start} · {context}")
                };
                lines.push(DiffLine {
                    kind: LineKind::Hunk,
                    old: None,
                    new: None,
                    spans: vec![Span::styled(clean(&label), muted)],
                });
            }
            (old, new) = (old_start, new_start);
            started = true;
            continue;
        }
        if !started {
            if [
                "rename ",
                "old mode",
                "new mode",
                "Binary files",
                "similarity",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
            {
                lines.push(DiffLine {
                    kind: LineKind::Note,
                    old: None,
                    new: None,
                    spans: vec![Span::styled(clean(line), muted)],
                });
            }
            continue;
        }
        if line.starts_with('\\') {
            continue;
        }
        let (kind, content) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            _ => (LineKind::Context, line),
        };
        let content = content.trim_end_matches('\r');
        let (old_number, new_number) = match kind {
            LineKind::Added => {
                new += 1;
                new_text.push_str(content);
                new_text.push('\n');
                (None, Some(new - 1))
            }
            LineKind::Removed => {
                old += 1;
                old_text.push_str(content);
                old_text.push('\n');
                (Some(old - 1), None)
            }
            _ => {
                old += 1;
                new += 1;
                old_text.push_str(content);
                old_text.push('\n');
                new_text.push_str(content);
                new_text.push('\n');
                (Some(old - 1), Some(new - 1))
            }
        };
        lines.push(DiffLine {
            kind,
            old: old_number,
            new: new_number,
            spans: vec![Span::styled(clean(content), text)],
        });
    }
    let language = loader
        .language_for_filename(path)
        .map(|language| loader.language(language).config().language_id.clone());
    if let Some(language) = language {
        let highlight = |source: &str| {
            crate::ui::markdown::highlighted_code_block(
                source,
                &language,
                Some(theme),
                loader,
                None,
            )
            .lines
        };
        let old_lines = highlight(&old_text);
        let new_lines = highlight(&new_text);
        let (mut old_index, mut new_index) = (0, 0);
        for line in &mut lines {
            let source = match line.kind {
                LineKind::Removed => {
                    old_index += 1;
                    old_lines.get(old_index - 1)
                }
                LineKind::Added => {
                    new_index += 1;
                    new_lines.get(new_index - 1)
                }
                LineKind::Context => {
                    old_index += 1;
                    new_index += 1;
                    new_lines.get(new_index - 1)
                }
                _ => continue,
            };
            if let Some(source) = source {
                line.spans = source
                    .0
                    .iter()
                    .map(|span| Span::styled(clean(&span.content), span.style))
                    .collect();
            }
        }
    }
    lines
}

/// Wrap display lines to `width`, with old/new line numbers and a sign gutter.
/// Row backgrounds span the full width so changes read as shaded bands.
fn diff_rows(lines: &[DiffLine], width: u16, colors: &DiffColors) -> Vec<Spans<'static>> {
    let width = width as usize;
    let digits = lines
        .iter()
        .filter_map(|line| line.old.max(line.new))
        .max()
        .unwrap_or(0)
        .to_string()
        .len();
    let gutter_width = digits * 2 + 4;
    let mut rows = Vec::new();
    if width == 0 {
        return rows;
    }
    let content_width = width.saturating_sub(gutter_width).max(1);
    let number = |value: Option<usize>| {
        value.map_or_else(|| " ".repeat(digits), |value| format!("{value:>digits$}"))
    };
    for line in lines {
        let background = colors.row(line.kind);
        let (sign, sign_style) = match line.kind {
            LineKind::Added => ("+", colors.plus),
            LineKind::Removed => ("-", colors.minus),
            _ => (" ", colors.gutter),
        };
        // Very narrow panes drop the gutter rather than overflow.
        let gutter =
            if matches!(line.kind, LineKind::Hunk | LineKind::Note) || width <= gutter_width {
                None
            } else {
                Some(vec![
                    Span::styled(
                        format!("{} {} ", number(line.old), number(line.new)),
                        colors.gutter.patch(background),
                    ),
                    Span::styled(format!("{sign} "), sign_style.patch(background)),
                ])
            };
        let available = if gutter.is_some() {
            content_width
        } else {
            width
        };
        let mut segments: Vec<Vec<Span<'static>>> = vec![Vec::new()];
        let mut used = 0;
        for span in &line.spans {
            let style = span.style.patch(background);
            for grapheme in span.content.graphemes(true) {
                let size = grapheme.width();
                if size > available {
                    continue;
                }
                if used + size > available {
                    segments.push(Vec::new());
                    used = 0;
                }
                let segment = segments.last_mut().unwrap();
                match segment.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push_str(grapheme),
                    _ => segment.push(Span::styled(grapheme.to_owned(), style)),
                }
                used += size;
            }
        }
        for (index, segment) in segments.into_iter().enumerate() {
            let mut spans = match (&gutter, index) {
                (Some(gutter), 0) => gutter.clone(),
                (Some(_), _) => vec![Span::styled(
                    " ".repeat(gutter_width.min(width)),
                    colors.gutter.patch(background),
                )],
                (None, _) => Vec::new(),
            };
            spans.extend(segment);
            let filled: usize = spans.iter().map(|span| span.content.width()).sum();
            if filled < width && background != Style::default() {
                spans.push(Span::styled(" ".repeat(width - filled), background));
            }
            rows.push(Spans::from(spans));
        }
    }
    rows
}

impl DiffPane {
    fn new(shared: SharedReview) -> Self {
        Self {
            shared,
            keys: keymap!({ "Changes"
                "q" => pane_close, "esc" | "tab" => focus_document_pane,
                "r" => pane_refresh, "o" | "ret" => pane_open_reference,
                "home" => goto_file_start, "end" | "G" => goto_last_line,
            }),
            offset: 0,
            rows: 0,
            height: 1,
            path: None,
            cache: None,
        }
    }
}

impl Pane for DiffPane {
    fn kind(&self) -> &'static str {
        ID
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn keymap(&self) -> &KeyTrie {
        &self.keys
    }
    fn command(&mut self, command: PaneCommand, count: usize) -> Result<PaneOutcome> {
        let last = self.rows.saturating_sub(self.height);
        match command {
            PaneCommand::Up => self.offset = self.offset.saturating_sub(count),
            PaneCommand::Down => self.offset = self.offset.saturating_add(count).min(last),
            PaneCommand::PageUp => {
                self.offset = self
                    .offset
                    .saturating_sub(self.height.saturating_mul(count))
            }
            PaneCommand::PageDown => {
                self.offset = self
                    .offset
                    .saturating_add(self.height.saturating_mul(count))
                    .min(last)
            }
            PaneCommand::First => self.offset = 0,
            PaneCommand::Last => self.offset = last,
            PaneCommand::Refresh => self.shared.lock().requested = true,
            PaneCommand::Close => return Ok(PaneOutcome::Close),
            PaneCommand::FocusDocument => {
                return Ok(PaneOutcome::Callback(Box::new(|compositor, cx| {
                    if let Some(view) = compositor.find::<super::EditorView>() {
                        if let Some(id) = view.panes.find(super::file_tree::ID) {
                            view.panes.focus_pane(id, cx.editor);
                        }
                    }
                })))
            }
            PaneCommand::OpenReference => {
                let review = self.shared.lock();
                let file = review
                    .files
                    .get(review.selected)
                    .context("No changed file selected")?;
                let target = super::opencode::references::FileTarget {
                    path: file.path.clone(),
                    line: None,
                    column: None,
                };
                let path = target.resolve(&review.root)?;
                return Ok(PaneOutcome::Callback(Box::new(move |compositor, cx| {
                    if let Err(error) =
                        super::opencode::references::open_file(cx.editor, &path, None)
                    {
                        cx.editor.set_error(error.to_string());
                        return;
                    }
                    if let Some(view) = compositor.find::<super::EditorView>() {
                        if let Some(id) = view.panes.find(ID) {
                            view.panes.remove(id, cx.editor);
                        }
                        view.panes.focus_document(cx.editor);
                    }
                })));
            }
            _ => (),
        }
        Ok(PaneOutcome::None)
    }
    fn mouse(&mut self, event: &MouseEvent, _area: Rect) {
        match event.kind {
            MouseEventKind::ScrollUp => self.offset = self.offset.saturating_sub(3),
            MouseEventKind::ScrollDown => {
                self.offset = (self.offset + 3).min(self.rows.saturating_sub(self.height))
            }
            _ => (),
        }
    }
    fn update(&mut self, _id: PaneId, editor: &Editor, jobs: &mut Jobs) {
        update(&self.shared, editor, jobs);
    }
    fn render(&mut self, area: Rect, surface: &mut Buffer, editor: &Editor, _focused: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let review = self.shared.lock();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                surface.set_string(x, y, " ", editor.theme.get("ui.background"));
            }
        }
        let file = review.files.get(review.selected);
        let path = file.map(|file| file.path.clone());
        if path != self.path {
            self.path = path;
            self.offset = 0;
        }
        let theme = &editor.theme;
        let statusline = theme.get("ui.statusline");
        surface.set_style(area.with_height(1), statusline);
        let mut title = vec![Span::styled(format!(" {} ", review.title), statusline)];
        if let Some(file) = file {
            title.push(Span::styled(
                format!(
                    "· {} {}  ",
                    super::file_tree::icon(&file.path, false, false),
                    file.path.display()
                ),
                statusline.add_modifier(Modifier::BOLD),
            ));
            if file.patch.is_some() {
                title.push(Span::styled(
                    format!("+{}", file.additions),
                    statusline.patch(Style {
                        fg: theme.get("diff.plus").fg,
                        ..Style::default()
                    }),
                ));
                title.push(Span::styled(
                    format!(" -{}", file.deletions),
                    statusline.patch(Style {
                        fg: theme.get("diff.minus").fg,
                        ..Style::default()
                    }),
                ));
            }
        }
        surface.set_spans(area.x, area.y, &Spans::from(title), area.width);
        let body = area.clip_top(1).clip_bottom(1);
        self.height = body.height.max(1) as usize;
        let error = review.error.as_deref().or_else(|| {
            review
                .patch_error
                .as_ref()
                .filter(|(path, _)| Some(path) == self.path.as_ref())
                .map(|(_, error)| error.as_str())
        });
        let message = if review.loading {
            Some("Loading changes…")
        } else if let Some(error) = error {
            Some(error)
        } else if file.is_none() {
            Some("No changed files")
        } else if file.is_some_and(|file| file.patch.is_none()) {
            Some("Loading file diff…")
        } else {
            None
        };
        if let Some(message) = message {
            self.rows = 0;
            write(
                surface,
                body.with_height(1).clip_left(1),
                message,
                theme.get("ui.text.inactive"),
            );
        } else if let Some((file, patch)) =
            file.and_then(|file| Some((file, file.patch.as_deref()?)))
        {
            let stale = self.cache.as_ref().is_none_or(|cache| {
                cache.path != file.path || cache.patch != patch || cache.theme != theme.name()
            });
            if stale {
                let lines = diff_lines(patch, &file.path, theme, &editor.syn_loader.load());
                self.cache = Some(Rendered {
                    path: file.path.clone(),
                    patch: patch.to_owned(),
                    theme: theme.name().to_owned(),
                    lines,
                    width: 0,
                    rows: Vec::new(),
                });
            }
            let cache = self.cache.as_mut().unwrap();
            if stale || cache.width != body.width {
                cache.rows = diff_rows(&cache.lines, body.width, &DiffColors::new(theme));
                cache.width = body.width;
            }
            self.rows = cache.rows.len();
            self.offset = self.offset.min(self.rows.saturating_sub(self.height));
            for (row, spans) in cache
                .rows
                .iter()
                .skip(self.offset)
                .take(body.height as usize)
                .enumerate()
            {
                surface.set_spans(body.x, body.y + row as u16, spans, body.width);
            }
        }
        if area.height > 1 {
            write(
                surface,
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
                " j/k scroll  Tab files  o open file  r refresh  q close",
                theme.get("ui.text.inactive"),
            );
        }
    }
    fn cursor(&self, _area: Rect) -> (Option<Position>, CursorKind) {
        (Some(Position::default()), CursorKind::Hidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, status: &str) -> ChangeFile {
        ChangeFile {
            path: path.into(),
            original: None,
            status: status.into(),
            patch: Some("-before\n+after\n".into()),
            additions: 1,
            deletions: 1,
        }
    }

    fn screen(surface: &Buffer) -> String {
        let area = surface.area;
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| surface.get(x, y).unwrap().symbol.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn changes_filter_contains_only_requested_files_including_deleted_paths_and_restores_the_tree(
    ) -> Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join("folder"))?;
        std::fs::write(root.path().join("folder/unchanged.rs"), "clean\n")?;
        std::fs::write(root.path().join("changed.rs"), "changed\n")?;
        let mut tree = FileTree::new(root.path().to_owned())?;
        tree.command(PaneCommand::Right, 1)?;
        let area = Rect::new(0, 0, 60, 12);
        let theme = &helix_view::theme::DEFAULT_THEME;
        let mut original = Buffer::empty(area);
        tree.render(area, &mut original, theme, true);
        let review = Review::new(root.path().to_owned(), "Selected turn".into(), Source::Git);
        {
            let mut state = review.lock();
            state.requested = false;
            state.files = vec![
                file("changed.rs", "modified"),
                file("deleted.rs", "deleted"),
            ];
        }
        tree.changes = Some(review.clone());
        let mut filtered = Buffer::empty(area);
        tree.render(area, &mut filtered, theme, true);
        let output = screen(&filtered);
        assert!(output.contains("changed.rs"));
        assert!(output.contains("deleted.rs"));
        assert!(!output.contains("folder"));
        assert!(!output.contains("unchanged.rs"));
        tree.command(PaneCommand::Down, 1)?;
        assert_eq!(review.lock().selected, 1);
        tree.changes = None;
        let mut restored = Buffer::empty(area);
        tree.render(area, &mut restored, theme, true);
        assert_eq!(
            screen(&original),
            screen(&restored),
            "The normal tree's expansion and selection must survive review"
        );
        Ok(())
    }

    #[test]
    fn diff_lines_number_both_sides_and_shade_changes_across_the_row() {
        let theme = &helix_view::theme::DEFAULT_THEME;
        let loader = helix_core::syntax::Loader::default();
        let patch = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1,3 +1,3 @@\n fn main() {\n-    old();\n+    new_é界();\x1b[31m\n }\n\\ No newline at end of file\n";
        let lines = diff_lines(patch, Path::new("x.rs"), theme, &loader);
        let kinds: Vec<_> = lines.iter().map(|line| line.kind).collect();
        assert_eq!(
            kinds,
            [
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Context
            ],
            "Headers are hidden and a hunk at line 1 needs no separator"
        );
        assert_eq!((lines[1].old, lines[1].new), (Some(2), None));
        assert_eq!((lines[2].old, lines[2].new), (None, Some(2)));
        assert_eq!((lines[3].old, lines[3].new), (Some(3), Some(3)));
        let colors = DiffColors::new(theme);
        for width in [0, 1, 4, 12, 60] {
            let rows = diff_rows(&lines, width, &colors);
            assert!(rows.iter().all(|row| row.width() <= width as usize
                && !row
                    .0
                    .iter()
                    .any(|span| span.content.chars().any(char::is_control))));
        }
        let rows = diff_rows(&lines, 60, &colors);
        let text = |row: &Spans| {
            row.0
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        assert_eq!(text(&rows[1]).trim_end(), "2   -     old();");
        assert_eq!(text(&rows[2]).trim_end(), "  2 +     new_é界();");
        assert_eq!(rows[2].width(), 60, "Added rows are shaded to the edge");
        assert!(rows[2]
            .0
            .iter()
            .all(|span| span.style.bg == colors.added.bg));
        assert!(rows[1]
            .0
            .iter()
            .all(|span| span.style.bg == colors.removed.bg));
        assert!(rows[0].0.iter().all(|span| span.style.bg.is_none()));
    }

    #[test]
    fn partial_hunks_show_gaps_and_metadata_only_patches_show_notes() {
        let theme = &helix_view::theme::DEFAULT_THEME;
        let loader = helix_core::syntax::Loader::default();
        let lines = diff_lines(
            "@@ -10,2 +10,2 @@ fn outer\n a\n-b\n+c\n@@ -40 +40 @@\n-d\n+e\n",
            Path::new("x.txt"),
            theme,
            &loader,
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.kind == LineKind::Hunk)
                .count(),
            2
        );
        assert_eq!(lines[1].old, Some(10));
        let notes = diff_lines(
            "Binary files a/x and b/x differ\n",
            Path::new("x"),
            theme,
            &loader,
        );
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].kind, LineKind::Note);
    }

    #[test]
    fn turn_bases_come_from_whole_file_patches_only() {
        let patch = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n keep\n-old\r\n+new\n end\n\\ No newline at end of file\n";
        assert_eq!(
            turn_base(patch).as_deref(),
            Some("keep\nold\r\nend".as_bytes()),
            "Context and removed lines, keeping CRLF and a missing final newline"
        );
        assert_eq!(
            turn_base("@@ -0,0 +1,2 @@\n+a\n+b\n").as_deref(),
            Some(&b""[..]),
            "Added files start empty"
        );
        assert_eq!(
            turn_base("@@ -5,2 +5,2 @@\n-a\n+b\n"),
            None,
            "Partial patches"
        );
        assert_eq!(
            turn_base("@@ -1 +1 @@\n-a\n+b\n@@ -9 +9 @@\n-c\n+d\n"),
            None,
            "Several hunks mean context was omitted"
        );
        assert_eq!(turn_base("Binary files differ\n"), None);
    }

    #[test]
    fn changed_files_are_grouped_into_collapsible_folders() -> Result<()> {
        let review = Review::new("/work".into(), "Test".into(), Source::Git);
        review.lock().files = vec![
            file("src/ui/b.rs", "modified"),
            file("README.md", "modified"),
            file("src/ui/a.rs", "added"),
            file("src/lib.rs", "deleted"),
        ];
        review.lock().selected = 0;
        review.lock().reveal_selected();
        let labels = |review: &SharedReview| {
            review
                .lock()
                .rows()
                .iter()
                .map(|row| format!("{}{}", "  ".repeat(row.depth), row.name))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(&review),
            [
                "src",
                "  ui",
                "    a.rs",
                "    b.rs",
                "  lib.rs",
                "README.md"
            ]
        );
        assert_eq!(
            review.lock().cursor,
            3,
            "The cursor follows the selected file"
        );
        list_command(&review, PaneCommand::Up, 1, 20)?;
        assert_eq!(review.lock().selected, 2);
        list_command(&review, PaneCommand::Up, 1, 20)?;
        assert_eq!(review.lock().cursor, 1);
        assert_eq!(review.lock().selected, 2, "Folders keep the current diff");
        list_command(&review, PaneCommand::Activate, 1, 20)?;
        assert_eq!(labels(&review), ["src", "  ui", "  lib.rs", "README.md"]);
        list_command(&review, PaneCommand::Right, 1, 20)?;
        assert_eq!(labels(&review).len(), 6);
        list_command(&review, PaneCommand::Left, 1, 20)?;
        list_command(&review, PaneCommand::Left, 1, 20)?;
        assert_eq!(
            review.lock().cursor,
            0,
            "Left on a collapsed folder moves to its parent"
        );
        let area = Rect::new(0, 0, 30, 8);
        let mut surface = Buffer::empty(area);
        render_list(
            &review,
            area,
            &mut surface,
            &helix_view::theme::DEFAULT_THEME,
            true,
        );
        let output = screen(&surface);
        assert!(output.contains("  \u{f07b} ui"), "{output}");
        assert!(output
            .lines()
            .any(|line| line.contains("README.md") && line.trim_end().ends_with('M')));
        Ok(())
    }

    #[test]
    fn deleted_files_and_escaping_symlinks_can_be_reviewed_but_not_opened_or_created() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        let shared = Review::new(root.path().to_owned(), "Test".into(), Source::Git);
        shared.lock().files = vec![file("deleted.rs", "deleted")];
        let mut pane = DiffPane::new(shared.clone());
        assert!(pane.command(PaneCommand::OpenReference, 1).is_err());
        assert!(!root.path().join("deleted.rs").exists());
        for path in ["../outside", "/etc/passwd", "", "."] {
            assert!(validate_relative(Path::new(path)).is_err());
        }
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir()?;
            std::fs::write(outside.path().join("secret"), "secret")?;
            std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("link"))?;
            shared.lock().files = vec![file("link", "modified")];
            assert!(pane.command(PaneCommand::OpenReference, 1).is_err());
        }
        Ok(())
    }

    #[test]
    fn tiny_filtered_lists_do_not_render_outside_their_area() {
        let shared = Review::new("/work".into(), "Test".into(), Source::Git);
        shared.lock().files = vec![file("source.rs", "modified")];
        for width in [0, 1, 2, 10] {
            for height in [0, 1, 2, 5] {
                let area = Rect::new(2, 2, width, height);
                let outer = Rect::new(0, 0, width + 4, height + 4);
                let mut surface = Buffer::empty(outer);
                for y in 0..outer.height {
                    surface.set_string(0, y, &"x".repeat(outer.width as usize), Style::default());
                }
                render_list(
                    &shared,
                    area,
                    &mut surface,
                    &helix_view::theme::DEFAULT_THEME,
                    true,
                );
                for y in 0..outer.height {
                    for x in 0..outer.width {
                        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
                            assert_eq!(surface.get(x, y).unwrap().symbol.as_str(), "x");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn turn_paths_are_relative_to_the_repository_but_open_only_inside_the_session_directory(
    ) -> Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join(".git"))?;
        std::fs::create_dir(root.path().join("sub"))?;
        let session = root.path().join("sub");
        assert_eq!(
            turn_file_path(&session, "sub/source.rs".into())?,
            Path::new("source.rs")
        );
        assert_eq!(
            turn_file_path(&session, session.join("source.rs"))?,
            Path::new("source.rs")
        );
        assert!(turn_file_path(&session, "outside.rs".into()).is_err());
        assert!(turn_file_path(&session, "sub/../outside.rs".into()).is_err());
        Ok(())
    }
}
