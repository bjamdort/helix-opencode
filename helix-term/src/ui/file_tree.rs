mod git;

use std::{
    collections::HashSet,
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use helix_core::Position;
use helix_view::{
    editor::Action,
    graphics::{CursorKind, Rect},
    input::{MouseButton, MouseEventKind},
    Editor, Theme,
};
use tui::buffer::Buffer as Surface;

use crate::{
    compositor::Event,
    ctrl,
    job::{Callback, Jobs},
    key,
};

struct Entry {
    path: PathBuf,
    directory: bool,
    depth: usize,
}

impl Entry {
    fn icon(&self, expanded: bool) -> &'static str {
        if self.directory {
            return if expanded { "\u{f07c}" } else { "\u{f07b}" };
        }

        let name = self
            .path
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

        let extension = self
            .path
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
        tree.render(area, &mut surface, theme);
        assert_eq!(
            surface.get(0, 1).unwrap().symbol.as_str(),
            tree.entries[0].icon(false)
        );
        tree.expand()?;
        let mut surface = Surface::empty(area);
        tree.render(area, &mut surface, theme);
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
        tree.render(area, &mut surface, theme);
        assert_eq!(surface.get(18, 0).unwrap().symbol.as_str(), "M");
        assert_eq!(surface.get(18, 1).unwrap().symbol.as_str(), "M");
        assert_eq!(surface.get(18, 2).unwrap().symbol.as_str(), " ");
        assert_eq!(surface.get(18, 3).unwrap().symbol.as_str(), "?");
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
        assert_eq!(surface.get(19, 1).unwrap().symbol.as_str(), "│");
        // Tiny terminals must never place a status marker outside the sidebar.
        for width in 0..5 {
            let area = Rect::new(0, 0, width, 3);
            tree.render(area, &mut Surface::empty(area), theme);
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
    pub focused: bool,
    pub close_requested: bool,
    entries: Vec<Entry>,
    expanded: HashSet<PathBuf>,
    selected: usize,
    offset: usize,
    area: Rect,
    window_prefix: bool,
    space_prefix: bool,
    git_status: git::Snapshot,
    git_pending: Option<Arc<()>>,
    git_last_refresh: Option<Instant>,
    git_refresh_requested: bool,
}

impl FileTree {
    pub fn new(root: PathBuf) -> io::Result<Self> {
        let mut tree = Self {
            root,
            focused: true,
            close_requested: false,
            entries: Vec::new(),
            expanded: HashSet::new(),
            selected: 0,
            offset: 0,
            area: Rect::default(),
            window_prefix: false,
            space_prefix: false,
            git_status: git::Snapshot::default(),
            git_pending: None,
            git_last_refresh: None,
            git_refresh_requested: true,
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

    fn activate(&mut self, editor: &mut Editor) -> anyhow::Result<()> {
        if let Some(entry) = self.entries.get(self.selected) {
            if entry.directory {
                if self.expanded.contains(&entry.path) {
                    self.collapse();
                } else {
                    self.expand()?;
                }
            } else {
                editor.open(&entry.path, Action::Replace)?;
                self.focused = false;
            }
        }
        Ok(())
    }

    /// Returns true when input belongs to the sidebar rather than the document.
    pub fn handle_event(&mut self, event: &Event, editor: &mut Editor) -> bool {
        if matches!(event, Event::FocusGained) {
            self.request_git_refresh();
        }
        if let Event::Mouse(mouse) = event {
            let inside = mouse.column >= self.area.x
                && mouse.column < self.area.right()
                && mouse.row >= self.area.y
                && mouse.row < self.area.bottom();
            if !inside {
                if matches!(mouse.kind, MouseEventKind::Down(_)) {
                    self.focused = false;
                    self.window_prefix = false;
                    self.space_prefix = false;
                }
                return false;
            }
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    editor.enter_normal_mode();
                    if !self.focused {
                        self.request_git_refresh();
                    }
                    self.focused = true;
                    let row = mouse.row.saturating_sub(self.area.y + 1) as usize;
                    self.selected = (self.offset + row).min(self.entries.len().saturating_sub(1));
                }
                MouseEventKind::ScrollDown => {
                    self.selected = (self.selected + 3).min(self.entries.len().saturating_sub(1));
                }
                MouseEventKind::ScrollUp => self.selected = self.selected.saturating_sub(3),
                _ => (),
            }
            return true;
        }
        if !self.focused {
            return false;
        }
        let Event::Key(mut key) = *event else {
            return matches!(event, Event::Paste(_));
        };
        super::editor::canonicalize_key(&mut key);
        editor.reset_idle_timer();
        if self.window_prefix {
            self.window_prefix = false;
            if matches!(
                key,
                key!('l') | key!(Right) | ctrl!('l') | key!('w') | ctrl!('w')
            ) {
                self.focused = false;
            } else if matches!(key, key!('q') | ctrl!('q')) {
                self.close_requested = true;
            }
            return true;
        }
        if self.space_prefix {
            self.space_prefix = false;
            self.window_prefix = key == key!('w');
            self.close_requested = key == key!('E');
            return true;
        }
        let result: anyhow::Result<()> = match key {
            key!('j') | key!(Down) => {
                self.selected = (self.selected + 1).min(self.entries.len().saturating_sub(1));
                Ok(())
            }
            key!('k') | key!(Up) => {
                self.selected = self.selected.saturating_sub(1);
                Ok(())
            }
            key!('g') | key!(Home) => {
                self.selected = 0;
                Ok(())
            }
            key!('G') | key!(End) => {
                self.selected = self.entries.len().saturating_sub(1);
                Ok(())
            }
            key!('l') | key!(Right) => self.expand().map_err(Into::into),
            key!('h') | key!(Left) => {
                self.collapse();
                Ok(())
            }
            key!(Enter) => self.activate(editor),
            key!('r') => self.refresh().map_err(Into::into),
            key!(Esc) | key!(Tab) => {
                self.focused = false;
                Ok(())
            }
            ctrl!('w') => {
                self.window_prefix = true;
                Ok(())
            }
            key!('q') => {
                self.close_requested = true;
                Ok(())
            }
            key!(' ') => {
                self.space_prefix = true;
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            editor.set_error(format!("File explorer: {error}"));
        }
        true
    }

    pub fn request_git_refresh(&mut self) {
        self.git_refresh_requested = true;
    }

    pub fn update_git_status(&mut self, editor: &Editor, jobs: &mut Jobs) {
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
                        .and_then(|editor| editor.file_tree_mut())
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

    pub fn render(&mut self, area: Rect, surface: &mut Surface, theme: &Theme) {
        self.area = area;
        if area.width < 2 || area.height == 0 {
            self.focused = false;
            return;
        }
        let height = area.height.saturating_sub(1) as usize;
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected.saturating_sub(height.saturating_sub(1));
        }
        surface.set_style(area, theme.get("ui.background"));
        let width = area.width - 1;
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
            if self.focused && self.offset + row == self.selected {
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
        for y in area.y..area.bottom() {
            surface.set_string(area.right() - 1, y, "│", theme.get("ui.window"));
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
