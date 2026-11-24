pub(super) mod git;

use std::{
    collections::HashSet,
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use helix_core::{hashmap, Position};
use helix_view::{
    editor::Action,
    graphics::{CursorKind, Rect},
    input::{MouseButton, MouseEventKind},
    Editor, PaneId, Theme,
};
use tui::buffer::Buffer as Surface;

use super::pane::{Pane, PaneOutcome};
use crate::{
    commands::pane::PaneCommand,
    job::{Callback, Jobs},
    keymap::{macros::keymap, KeyTrie},
};

pub(super) const ID: &str = "file-tree";

struct Entry {
    path: PathBuf,
    directory: bool,
    depth: usize,
}

impl Entry {
    fn icon(&self, expanded: bool) -> &'static str {
        icon(&self.path, self.directory, expanded)
    }
}

/// A Nerd Font icon for a directory or file, shared with the changes list.
pub(super) fn icon(path: &std::path::Path, directory: bool, expanded: bool) -> &'static str {
    if directory {
        return if expanded { "\u{f07c}" } else { "\u{f07b}" };
    }

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    // Specific filenames take precedence over their extensions.
    match name {
        "Cargo.toml" | "Cargo.lock" => return "\u{e7a8}",
        "Dockerfile" | "Containerfile" | "docker-compose.yml" | "docker-compose.yaml" => {
            return "\u{f308}"
        }
        "Makefile" | "GNUmakefile" | "justfile" | "Justfile" => return "\u{e615}",
        "LICENSE" | "LICENSE.md" | "LICENSE.txt" | "COPYING" => return "\u{f48a}",
        ".gitignore" | ".gitattributes" | ".gitmodules" => return "\u{e702}",
        _ => (),
    }

    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "rs" => "\u{e7a8}",
        "js" | "mjs" | "cjs" | "jsx" => "\u{e74e}",
        "ts" | "mts" | "cts" | "tsx" => "\u{e628}",
        "py" | "pyi" | "pyw" => "\u{e73c}",
        "go" => "\u{e627}",
        "c" | "h" => "\u{e61e}",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "\u{e61d}",
        "java" | "class" | "jar" => "\u{e738}",
        "rb" | "gemspec" => "\u{e739}",
        "php" => "\u{e73d}",
        "lua" => "\u{e620}",
        "ex" | "exs" => "\u{e62d}",
        "swift" => "\u{e755}",
        "nix" => "\u{f313}",
        "sh" | "bash" | "zsh" | "fish" => "\u{f489}",
        "html" | "htm" => "\u{e736}",
        "css" | "scss" | "sass" | "less" => "\u{e749}",
        "json" | "jsonc" => "\u{e60b}",
        "toml" | "yaml" | "yml" | "ini" | "conf" | "cfg" => "\u{e615}",
        "md" | "mdx" | "markdown" => "\u{e73e}",
        "txt" | "rst" | "adoc" => "\u{f15c}",
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp" => "\u{f1c5}",
        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "zst" => "\u{f1c6}",
        "pdf" => "\u{f1c1}",
        "ttf" | "otf" | "woff" | "woff2" => "\u{f031}",
        _ => "\u{f15b}",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_folder_icons_without_disclosure_markers() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join("folder"))?;
        std::fs::write(root.path().join("folder/file"), "file")?;
        let mut tree = FileTree::new(root.path().to_owned())?;
        let area = Rect::new(0, 0, 20, 4);
        let theme = &helix_view::theme::DEFAULT_THEME;
        let mut surface = Surface::empty(area);
        tree.render(area, &mut surface, theme, true);
        assert_eq!(
            surface.get(0, 1).unwrap().symbol.as_str(),
            tree.entries[0].icon(false)
        );
        tree.expand()?;
        let mut surface = Surface::empty(area);
        tree.render(area, &mut surface, theme, true);
        assert_eq!(
            surface.get(0, 1).unwrap().symbol.as_str(),
            tree.entries[0].icon(true)
        );
        assert_eq!(surface.get(0, 2).unwrap().symbol.as_str(), " ");
        assert_eq!(surface.get(1, 2).unwrap().symbol.as_str(), " ");
        assert_eq!(
            surface.get(2, 2).unwrap().symbol.as_str(),
            tree.entries[1].icon(false)
        );
        Ok(())
    }

    #[test]
    fn renders_git_colors_markers_and_selection_background() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        for name in ["a", "b", "c"] {
            std::fs::write(root.path().join(name), name)?;
        }
        let mut tree = FileTree::new(root.path().to_owned())?;
        tree.git_status = git::Snapshot::parse(b" M a\0?? c\0", std::path::Path::new(""))?;
        let theme = &helix_view::theme::DEFAULT_THEME;
        let area = Rect::new(0, 0, 20, 5);
        let mut surface = Surface::empty(area);
        tree.render(area, &mut surface, theme, true);
        assert_eq!(surface.get(19, 0).unwrap().symbol.as_str(), "M");
        assert_eq!(surface.get(19, 1).unwrap().symbol.as_str(), "M");
        assert_eq!(surface.get(19, 2).unwrap().symbol.as_str(), " ");
        assert_eq!(surface.get(19, 3).unwrap().symbol.as_str(), "?");
        assert_eq!(
            surface.get(4, 1).unwrap().fg,
            theme.get("diff.delta").fg.unwrap()
        );
        assert_eq!(
            surface.get(4, 1).unwrap().bg,
            theme.get("ui.selection").bg.unwrap()
        );
        assert_eq!(
            surface.get(4, 3).unwrap().fg,
            theme.get("diff.plus").fg.unwrap()
        );
        // Tiny terminals must never place a status marker outside the sidebar.
        for width in 0..5 {
            let area = Rect::new(0, 0, width, 3);
            tree.render(area, &mut Surface::empty(area), theme, true);
        }
        Ok(())
    }

    #[test]
    fn icons_distinguish_folder_states_and_file_types() {
        let mut entry = Entry {
            path: "folder.rs".into(),
            directory: true,
            depth: 0,
        };
        assert_ne!(entry.icon(false), entry.icon(true));
        entry.directory = false;
        let rust_icon = entry.icon(false);
        assert_eq!(rust_icon, entry.icon(true));
        for name in ["Cargo.toml", "Cargo.lock", "main.RS"] {
            entry.path = name.into();
            assert_eq!(entry.icon(false), rust_icon);
        }
        entry.path = "main.py".into();
        assert_ne!(entry.icon(false), rust_icon);
        for name in ["unknown.extension", "no-extension", ".hidden"] {
            entry.path = name.into();
            assert_eq!(entry.icon(false), "\u{f15b}");
        }
    }

    #[test]
    fn directories_sort_first_and_expand_lazily() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("a.txt"), "a")?;
        std::fs::create_dir(root.path().join("z"))?;
        std::fs::write(root.path().join("z/child.txt"), "child")?;
        let mut tree = FileTree::new(root.path().to_owned())?;
        assert_eq!(tree.entries.len(), 2);
        assert!(tree.entries[0].directory);
        tree.expand()?;
        assert_eq!(tree.entries.len(), 3);
        assert_eq!(tree.entries[1].depth, 1);
        tree.selected = 1;
        tree.collapse();
        assert_eq!(tree.selected, 0);
        tree.collapse();
        assert_eq!(tree.entries.len(), 2);
        Ok(())
    }

    #[test]
    fn refresh_preserves_selection_and_clamps_after_deletion() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("b"), "b")?;
        let mut tree = FileTree::new(root.path().to_owned())?;
        std::fs::write(root.path().join("a"), "a")?;
        tree.refresh()?;
        assert_eq!(tree.selected, 1);
        std::fs::remove_file(root.path().join("b"))?;
        tree.refresh()?;
        assert_eq!(tree.selected, 0);
        std::fs::remove_file(root.path().join("a"))?;
        tree.refresh()?;
        assert!(tree.entries.is_empty());
        tree.expand()?;
        tree.collapse();
        Ok(())
    }

    #[test]
    fn failed_expansion_does_not_change_tree() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let child = root.path().join("child");
        std::fs::create_dir(&child)?;
        let mut tree = FileTree::new(root.path().to_owned())?;
        std::fs::remove_dir(&child)?;
        assert!(tree.expand().is_err());
        assert!(tree.expanded.is_empty());
        assert_eq!(tree.entries.len(), 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlinks_are_not_expanded() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        std::os::unix::fs::symlink(root.path(), root.path().join("loop"))?;
        let mut tree = FileTree::new(root.path().to_owned())?;
        assert!(!tree.entries[0].directory);
        tree.expand()?;
        assert_eq!(tree.entries.len(), 1);
        Ok(())
    }
}

/// A persistent sidebar, independent of the document split tree.
pub(super) struct FileTree {
    pub root: PathBuf,
    keymap: KeyTrie,
    entries: Vec<Entry>,
    expanded: HashSet<PathBuf>,
    selected: usize,
    offset: usize,
    area: Rect,
    git_status: git::Snapshot,
    git_pending: Option<Arc<()>>,
    git_last_refresh: Option<Instant>,
    git_refresh_requested: bool,
    pub(super) changes: Option<super::changes::SharedReview>,
}

impl FileTree {
    pub fn new(root: PathBuf) -> io::Result<Self> {
        let mut tree = Self {
            root,
            keymap: keymap!({ "File explorer"
                "ret" => pane_activate,
                "\\" => pane_open_horizontal_split,
                "|" => pane_open_vertical_split,
                "r" => pane_refresh,
                "q" => pane_close,
                "esc" | "tab" => focus_document_pane,
                "home" => goto_file_start,
                "end" | "G" => goto_last_line,
            }),
            entries: Vec::new(),
            expanded: HashSet::new(),
            selected: 0,
            offset: 0,
            area: Rect::default(),
            git_status: git::Snapshot::default(),
            git_pending: None,
            git_last_refresh: None,
            git_refresh_requested: true,
            changes: None,
        };
        tree.refresh()?;
        Ok(tree)
    }

    fn children(path: &std::path::Path, depth: usize) -> io::Result<Vec<Entry>> {
        let mut entries = std::fs::read_dir(path)?
            .map(|entry| {
                let entry = entry?;
                // Do not follow directory symlinks: an expanded tree must not contain cycles.
                let directory = entry.file_type()?.is_dir();
                Ok(Entry {
                    path: entry.path(),
                    directory,
                    depth,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        entries.sort_by(|a, b| {
            b.directory
                .cmp(&a.directory)
                .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
        });
        Ok(entries)
    }

    fn refresh(&mut self) -> io::Result<()> {
        self.request_git_refresh();
        let selected = self
            .entries
            .get(self.selected)
            .map(|entry| entry.path.clone());
        let mut entries = Self::children(&self.root, 0)?;
        let mut index = 0;
        while index < entries.len() {
            let entry = &entries[index];
            if entry.directory && self.expanded.contains(&entry.path) {
                let children = Self::children(&entry.path, entry.depth + 1)?;
                entries.splice(index + 1..index + 1, children);
            }
            index += 1;
        }
        self.entries = entries;
        self.selected = selected
            .and_then(|path| self.entries.iter().position(|entry| entry.path == path))
            .unwrap_or(self.selected)
            .min(self.entries.len().saturating_sub(1));
        Ok(())
    }

    fn expand(&mut self) -> io::Result<()> {
        let Some(entry) = self.entries.get(self.selected) else {
            return Ok(());
        };
        if entry.directory && !self.expanded.contains(&entry.path) {
            let children = Self::children(&entry.path, entry.depth + 1)?;
            self.expanded.insert(entry.path.clone());
            self.entries
                .splice(self.selected + 1..self.selected + 1, children);
        }
        Ok(())
    }

    fn collapse(&mut self) {
        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };
        let depth = entry.depth;
        if self.expanded.remove(&entry.path) {
            let end = self.entries[self.selected + 1..]
                .iter()
                .position(|entry| entry.depth <= depth)
                .map_or(self.entries.len(), |index| self.selected + 1 + index);
            self.entries.drain(self.selected + 1..end);
        } else if depth > 0 {
            if let Some(parent) = self.entries[..self.selected]
                .iter()
                .rposition(|entry| entry.depth < depth)
            {
                self.selected = parent;
            }
        }
    }

    fn activate(&mut self) -> anyhow::Result<PaneOutcome> {
        self.open_selected(Action::Replace)
    }

    fn open_selected(&mut self, action: Action) -> anyhow::Result<PaneOutcome> {
        if let Some(entry) = self.entries.get(self.selected) {
            if entry.directory {
                if self.expanded.contains(&entry.path) {
                    self.collapse();
                } else {
                    self.expand()?;
                }
            } else {
                return Ok(PaneOutcome::OpenFile(entry.path.clone(), action));
            }
        }
        Ok(PaneOutcome::None)
    }

    pub fn request_git_refresh(&mut self) {
        self.git_refresh_requested = true;
    }

    pub fn update_git_status(&mut self, id: PaneId, editor: &Editor, jobs: &mut Jobs) {
        if !cfg!(feature = "git") || self.git_pending.is_some() {
            return;
        }
        if !self.git_refresh_requested
            && self
                .git_last_refresh
                .is_some_and(|last| last.elapsed() < Duration::from_secs(2))
        {
            return;
        }
        self.git_refresh_requested = false;
        self.git_last_refresh = Some(Instant::now());
        let workspace = helix_loader::find_workspace_in(&self.root).0;
        let trusted = editor
            .workspace_trust
            .query(&workspace, helix_loader::workspace_trust::TrustQuery::Git)
            .is_trusted();
        if !trusted {
            self.git_status = git::Snapshot::default();
            return;
        }
        let root = self.root.clone();
        // A unique token prevents an old job from updating a closed/reopened tree.
        let token = Arc::new(());
        self.git_pending = Some(token.clone());
        jobs.callback(async move {
            let result = git::Snapshot::load(&root).await;
            Ok(Callback::EditorCompositor(Box::new(
                move |editor, compositor| {
                    let Some(tree) = compositor
                        .find::<super::EditorView>()
                        .and_then(|editor| editor.panes.get_mut::<FileTree>(id))
                    else {
                        return;
                    };
                    if !tree
                        .git_pending
                        .as_ref()
                        .is_some_and(|pending| Arc::ptr_eq(pending, &token))
                    {
                        return;
                    }
                    tree.git_pending = None;
                    tree.git_last_refresh = Some(Instant::now());
                    if !editor
                        .workspace_trust
                        .query(&workspace, helix_loader::workspace_trust::TrustQuery::Git)
                        .is_trusted()
                    {
                        tree.git_status = git::Snapshot::default();
                        return;
                    }
                    tree.git_status = match result {
                        Ok(snapshot) => snapshot,
                        Err(error) => {
                            log::debug!("File explorer Git status: {error:#}");
                            git::Snapshot::default()
                        }
                    };
                },
            )))
        });
    }

    pub fn render(&mut self, area: Rect, surface: &mut Surface, theme: &Theme, focused: bool) {
        self.area = area;
        if let Some(review) = &self.changes {
            super::changes::render_list(review, area, surface, theme, focused);
            return;
        }
        if area.width < 2 || area.height == 0 {
            return;
        }
        let height = area.height.saturating_sub(1) as usize;
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected.saturating_sub(height.saturating_sub(1));
        }
        surface.set_style(area, theme.get("ui.background"));
        let width = area.width;
        let root_status = self.git_status.status(std::path::Path::new(""));
        let mut title_style = theme.get("ui.statusline");
        if let Some(color) = root_status.and_then(|status| theme.get(status.scope()).fg) {
            title_style = title_style.fg(color);
        }
        surface.set_stringn(
            area.x,
            area.y,
            &self.root.to_string_lossy(),
            width.saturating_sub(2) as usize,
            title_style,
        );
        if width >= 2 {
            if let Some(status) = root_status {
                surface.set_string(area.x + width - 1, area.y, status.marker(), title_style);
            }
        }
        for (row, entry) in self
            .entries
            .iter()
            .skip(self.offset)
            .take(height)
            .enumerate()
        {
            let y = area.y + 1 + row as u16;
            let status = entry
                .path
                .strip_prefix(&self.root)
                .ok()
                .and_then(|path| self.git_status.status(path));
            let mut style = if entry.directory {
                theme.get("ui.text.directory")
            } else {
                theme.get("ui.text")
            };
            if focused && self.offset + row == self.selected {
                style = style
                    .patch(theme.get("ui.text.focus"))
                    .patch(theme.get("ui.selection"));
            }
            // Keep the selection background, but retain the Git foreground on selected rows.
            if let Some(color) = status.and_then(|status| theme.get(status.scope()).fg) {
                style = style.fg(color);
            }
            surface.set_style(Rect::new(area.x, y, width, 1), style);
            let label = format!(
                "{}{} {}",
                "  ".repeat(entry.depth),
                entry.icon(self.expanded.contains(&entry.path)),
                entry.path.file_name().unwrap_or_default().to_string_lossy()
            );
            let label_width = width.saturating_sub(2);
            surface.set_stringn(area.x, y, &label, label_width as usize, style);
            if width >= 2 {
                if let Some(status) = status {
                    surface.set_string(area.x + width - 1, y, status.marker(), style);
                }
            }
        }
    }

    pub fn cursor(&self) -> (Option<Position>, CursorKind) {
        let row = self.selected.saturating_sub(self.offset) as u16 + 1;
        if self.area.width > 1 && row < self.area.height {
            (
                Some(Position::new(
                    (self.area.y + row) as usize,
                    self.area.x as usize,
                )),
                CursorKind::Hidden,
            )
        } else {
            (
                Some(Position::new(self.area.y as usize, self.area.x as usize)),
                CursorKind::Hidden,
            )
        }
    }
}

impl Pane for FileTree {
    fn kind(&self) -> &'static str {
        ID
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn keymap(&self) -> &KeyTrie {
        &self.keymap
    }

    fn command(&mut self, command: PaneCommand, count: usize) -> anyhow::Result<PaneOutcome> {
        if let Some(review) = &self.changes {
            return super::changes::list_command(review, command, count, self.area.height);
        }
        let last = self.entries.len().saturating_sub(1);
        let page = (self.area.height.saturating_sub(1) as usize).saturating_mul(count);
        match command {
            PaneCommand::Up => self.selected = self.selected.saturating_sub(count),
            PaneCommand::Down => self.selected = self.selected.saturating_add(count).min(last),
            PaneCommand::Left => self.collapse(),
            PaneCommand::Right => self.expand()?,
            PaneCommand::First => self.selected = count.saturating_sub(1).min(last),
            PaneCommand::Last => self.selected = last,
            PaneCommand::PageUp => self.selected = self.selected.saturating_sub(page),
            PaneCommand::PageDown => self.selected = self.selected.saturating_add(page).min(last),
            PaneCommand::Activate => return self.activate(),
            PaneCommand::OpenHorizontalSplit => return self.open_selected(Action::HorizontalSplit),
            PaneCommand::OpenVerticalSplit => return self.open_selected(Action::VerticalSplit),
            PaneCommand::Refresh => self.refresh()?,
            PaneCommand::Close => return Ok(PaneOutcome::Close),
            PaneCommand::FocusDocument => return Ok(PaneOutcome::FocusDocument),
            PaneCommand::Compose
            | PaneCommand::Interrupt
            | PaneCommand::New
            | PaneCommand::ToggleDetails
            | PaneCommand::Requests
            | PaneCommand::Review
            | PaneCommand::NextReference
            | PaneCommand::PreviousReference
            | PaneCommand::OpenReference
            | PaneCommand::References
            | PaneCommand::NextMessage
            | PaneCommand::PreviousMessage => {
                anyhow::bail!("This action is not supported by the file explorer")
            }
        }
        Ok(PaneOutcome::None)
    }

    fn mouse(&mut self, mouse: &helix_view::input::MouseEvent, area: Rect) {
        if let Some(review) = &self.changes {
            super::changes::list_mouse(review, mouse, area);
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if mouse.row > area.y => {
                let row = (mouse.row - area.y - 1) as usize;
                self.selected = (self.offset + row).min(self.entries.len().saturating_sub(1));
            }
            MouseEventKind::ScrollDown => {
                self.selected = (self.selected + 3).min(self.entries.len().saturating_sub(1))
            }
            MouseEventKind::ScrollUp => self.selected = self.selected.saturating_sub(3),
            _ => (),
        }
    }

    fn update(&mut self, id: PaneId, editor: &Editor, jobs: &mut Jobs) {
        if let Some(review) = &self.changes {
            super::changes::update(review, editor, jobs);
            return;
        }
        self.update_git_status(id, editor, jobs);
    }
    fn refresh(&mut self) {
        if self.changes.is_none() {
            self.request_git_refresh();
        }
    }
    fn render(&mut self, area: Rect, surface: &mut Surface, editor: &Editor, focused: bool) {
        self.render(area, surface, &editor.theme, focused);
    }
    fn cursor(&self, _area: Rect) -> (Option<Position>, CursorKind) {
        self.cursor()
    }
}
