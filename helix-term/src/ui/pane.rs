//! Frontend pane hosting. Document splits keep their existing View/Document model;
//! auxiliary panes share focus, layout, command routing, and window navigation.
use std::any::Any;

use helix_core::Position;
use helix_view::{
    editor::Action,
    graphics::{CursorKind, Rect},
    input::{MouseEvent, MouseEventKind},
    tree::Direction,
    Editor, PaneId, ViewId,
};
use slotmap::SlotMap;
use tui::buffer::Buffer;

use crate::{commands::pane::PaneCommand, job::Jobs, keymap::KeyTrie};

pub(crate) enum PaneOutcome {
    None,
    Close,
    FocusDocument,
    Overlay(Box<dyn crate::compositor::Component>),
    OpenFile(std::path::PathBuf, Action),
    OpenFileAt {
        path: std::path::PathBuf,
        position: Option<Position>,
    },
    Compose {
        title: &'static str,
        draft: std::sync::Arc<parking_lot::Mutex<String>>,
        anchor: Option<super::prompt::SharedPromptAnchor>,
    },
}

/// Content-specific behavior only: no global key decoding or focus ownership.
pub(crate) trait Pane: Any {
    fn kind(&self) -> &'static str;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn keymap(&self) -> &KeyTrie;
    fn max_width(&self, available: u16) -> u16 {
        available / 3
    }
    fn submit(&mut self, _text: String) -> anyhow::Result<()> {
        anyhow::bail!("This pane does not accept text");
    }
    fn command(&mut self, command: PaneCommand, count: usize) -> anyhow::Result<PaneOutcome>;
    fn mouse(&mut self, event: &MouseEvent, area: Rect);
    fn update(&mut self, id: PaneId, editor: &Editor, jobs: &mut Jobs);
    fn refresh(&mut self) {}
    fn render(&mut self, area: Rect, surface: &mut Buffer, editor: &Editor, focused: bool);
    fn cursor(&self, area: Rect) -> (Option<Position>, CursorKind);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dock {
    Left,
    Right,
}

struct HostedPane {
    content: Box<dyn Pane>,
    dock: Dock,
    width: u16,
    area: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusTarget {
    Document(ViewId),
    Pane(PaneId),
}

pub(crate) struct PaneManager {
    panes: SlotMap<PaneId, HostedPane>,
}

impl Default for PaneManager {
    fn default() -> Self {
        Self {
            panes: SlotMap::with_key(),
        }
    }
}

impl PaneManager {
    pub fn insert(
        &mut self,
        pane: impl Pane,
        dock: Dock,
        width: u16,
        editor: &mut Editor,
    ) -> PaneId {
        let id = self.panes.insert(HostedPane {
            content: Box::new(pane),
            dock,
            width,
            area: Rect::default(),
        });
        self.focus(FocusTarget::Pane(id), editor);
        id
    }

    pub fn find(&self, kind: &str) -> Option<PaneId> {
        self.panes
            .iter()
            .find_map(|(id, pane)| (pane.content.kind() == kind).then_some(id))
    }

    pub fn get_mut<T: Pane>(&mut self, id: PaneId) -> Option<&mut T> {
        self.panes.get_mut(id)?.content.as_any_mut().downcast_mut()
    }

    pub fn keymap(&self, editor: &Editor) -> Option<&KeyTrie> {
        self.panes
            .get(editor.focused_pane?)
            .map(|pane| pane.content.keymap())
    }

    pub fn focus_pane(&mut self, id: PaneId, editor: &mut Editor) {
        self.focus(FocusTarget::Pane(id), editor);
    }

    fn focus(&mut self, target: FocusTarget, editor: &mut Editor) {
        editor.enter_normal_mode();
        if !editor.should_close() {
            let (view, doc) = helix_view::current!(editor);
            doc.append_changes_to_history(view);
        }
        match target {
            FocusTarget::Document(id) => {
                editor.focused_pane = None;
                editor.focus(id);
            }
            FocusTarget::Pane(id) => {
                if let Some(pane) = self.panes.get_mut(id) {
                    editor.focused_pane = Some(id);
                    pane.content.refresh();
                }
            }
        }
    }

    pub fn focus_document(&mut self, editor: &mut Editor) {
        self.focus(FocusTarget::Document(editor.tree.focus), editor);
    }

    pub fn remove(&mut self, id: PaneId, editor: &mut Editor) {
        self.panes.remove(id);
        if editor.focused_pane == Some(id) {
            self.focus_document(editor);
        }
    }

    /// Reserve docked regions, returning the region for the document split tree.
    pub fn layout(&mut self, mut area: Rect, focused: &mut Option<PaneId>) -> Rect {
        for (id, pane) in &mut self.panes {
            let width = if area.width >= 20 {
                pane.width.min(pane.content.max_width(area.width))
            } else {
                0
            };
            pane.area = match pane.dock {
                Dock::Left => {
                    let pane_area = area.with_width(width);
                    area = area.clip_left(width);
                    pane_area
                }
                Dock::Right => {
                    let pane_area = Rect::new(area.right() - width, area.y, width, area.height);
                    area = area.with_width(area.width - width);
                    pane_area
                }
            };
            if (width == 0 || area.height == 0) && *focused == Some(id) {
                *focused = None;
            }
        }
        area
    }

    pub fn render(&mut self, surface: &mut Buffer, editor: &Editor, jobs: &mut Jobs) {
        for (id, pane) in &mut self.panes {
            pane.content.update(id, editor, jobs);
            if pane.area.width > 0 && pane.area.height > 0 {
                pane.content
                    .render(pane.area, surface, editor, editor.focused_pane == Some(id));
            }
        }
    }

    pub fn cursor(&self, editor: &Editor) -> Option<(Option<Position>, CursorKind)> {
        let pane = self.panes.get(editor.focused_pane?)?;
        Some(pane.content.cursor(pane.area))
    }

    pub fn refresh(&mut self) {
        for pane in self.panes.values_mut() {
            pane.content.refresh();
        }
    }

    pub fn mouse(&mut self, event: &MouseEvent, editor: &mut Editor) -> bool {
        let target = self.panes.iter().find_map(|(id, pane)| {
            (pane.area.width > 0
                && event.column >= pane.area.x
                && event.column < pane.area.right()
                && event.row >= pane.area.y
                && event.row < pane.area.bottom())
            .then_some(id)
        });
        if let Some(id) = target {
            if matches!(event.kind, MouseEventKind::Down(_)) {
                self.focus_pane(id, editor);
            }
            let pane = &mut self.panes[id];
            pane.content.mouse(event, pane.area);
            true
        } else {
            if editor.focused_pane.is_some() && matches!(event.kind, MouseEventKind::Down(_)) {
                self.focus_document(editor);
            }
            false
        }
    }

    pub fn command(
        &mut self,
        command: PaneCommand,
        count: usize,
        editor: &mut Editor,
    ) -> Option<crate::compositor::Callback> {
        let id = editor.focused_pane?;
        match command {
            PaneCommand::Close => {
                self.remove(id, editor);
                return None;
            }
            PaneCommand::FocusDocument => {
                self.focus_document(editor);
                return None;
            }
            _ => (),
        }
        let Some(pane) = self.panes.get_mut(id) else {
            editor.focused_pane = None;
            return None;
        };
        match pane.content.command(command, count) {
            Ok(PaneOutcome::None) => (),
            Ok(PaneOutcome::Close) => self.remove(id, editor),
            Ok(PaneOutcome::FocusDocument) => self.focus_document(editor),
            Ok(PaneOutcome::Overlay(component)) => {
                return Some(Box::new(move |compositor, _| {
                    compositor.push(component);
                }))
            }
            Ok(PaneOutcome::OpenFile(path, action)) => match editor.open(&path, action) {
                Ok(_) => self.focus_document(editor),
                Err(error) => editor.set_error(error.to_string()),
            },
            Ok(PaneOutcome::OpenFileAt { path, position }) => {
                match super::opencode::references::open_file(editor, &path, position) {
                    Ok(()) => self.focus_document(editor),
                    Err(error) => editor.set_error(error.to_string()),
                }
            }
            Ok(PaneOutcome::Compose {
                title,
                draft,
                anchor,
            }) => {
                return Some(Box::new(move |compositor, cx| {
                    let initial = draft.lock().clone();
                    let updates = draft.clone();
                    let mut prompt = super::Prompt::new(
                        if anchor.is_some() {
                            "".into()
                        } else {
                            title.into()
                        },
                        None,
                        |_, _| Vec::new(),
                        move |_, text, _| {
                            *updates.lock() = text.to_owned();
                        },
                    )
                    .with_line(initial, cx.editor)
                    .with_validate_callback(move |_, text| {
                        let text = text.to_owned();
                        *draft.lock() = text.clone();
                        let draft = draft.clone();
                        Some(Box::new(move |compositor, cx| {
                            let result = compositor
                                .find::<super::EditorView>()
                                .and_then(|view| view.panes.panes.get_mut(id))
                                .ok_or_else(|| anyhow::anyhow!("The pane has closed"))
                                .and_then(|pane| pane.content.submit(text));
                            match result {
                                Ok(()) => draft.lock().clear(),
                                Err(error) => cx.editor.set_error(error.to_string()),
                            }
                        }))
                    });
                    if let Some(anchor) = anchor {
                        prompt = prompt.with_anchor(anchor);
                    }
                    compositor.push(Box::new(prompt));
                }))
            }
            Err(error) => editor.set_error(format!("{}: {error}", pane.content.kind())),
        }
        None
    }

    fn targets(&self, editor: &Editor) -> Vec<(FocusTarget, Rect)> {
        let mut targets: Vec<_> = editor
            .tree
            .views()
            .map(|(view, _)| (FocusTarget::Document(view.id), view.area))
            .collect();
        targets.extend(
            self.panes
                .iter()
                .filter(|(_, pane)| pane.area.width > 0 && pane.area.height > 0)
                .map(|(id, pane)| (FocusTarget::Pane(id), pane.area)),
        );
        targets.sort_by_key(|(_, area)| (area.x, area.y));
        targets
    }

    fn focused(editor: &Editor) -> FocusTarget {
        editor
            .focused_pane
            .map_or(FocusTarget::Document(editor.tree.focus), FocusTarget::Pane)
    }

    pub fn rotate(&mut self, reverse: bool, editor: &mut Editor) {
        if self.panes.is_empty() {
            if reverse {
                editor.focus_prev();
            } else {
                editor.focus_next();
            }
            return;
        }
        let targets = self.targets(editor);
        let Some(index) = targets
            .iter()
            .position(|(target, _)| *target == Self::focused(editor))
        else {
            return;
        };
        let next = if reverse {
            (index + targets.len() - 1) % targets.len()
        } else {
            (index + 1) % targets.len()
        };
        self.focus(targets[next].0, editor);
    }

    pub fn focus_direction(&mut self, direction: Direction, editor: &mut Editor) {
        if self.panes.is_empty() {
            editor.focus_direction(direction);
            return;
        }
        let targets = self.targets(editor);
        let Some((_, current)) = targets
            .iter()
            .find(|(target, _)| *target == Self::focused(editor))
        else {
            return;
        };
        if let Some((target, _)) = targets
            .iter()
            .filter_map(|(target, area)| {
                directional_distance(*current, *area, direction).map(|distance| (*target, distance))
            })
            .min_by_key(|(_, distance)| *distance)
        {
            self.focus(target, editor);
        }
    }
}

fn directional_distance(from: Rect, to: Rect, direction: Direction) -> Option<(u16, u16)> {
    let overlap_y = from.y < to.bottom() && to.y < from.bottom();
    let overlap_x = from.x < to.right() && to.x < from.right();
    match direction {
        Direction::Left if overlap_y && to.right() <= from.x => {
            Some((from.x - to.right(), from.y.abs_diff(to.y)))
        }
        Direction::Right if overlap_y && to.x >= from.right() => {
            Some((to.x - from.right(), from.y.abs_diff(to.y)))
        }
        Direction::Up if overlap_x && to.bottom() <= from.y => {
            Some((from.y - to.bottom(), from.x.abs_diff(to.x)))
        }
        Direction::Down if overlap_x && to.y >= from.bottom() => {
            Some((to.y - from.bottom(), from.x.abs_diff(to.x)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A second content type verifies that the host has no file-tree dependency.
    struct TestPane {
        keymap: KeyTrie,
    }

    impl Pane for TestPane {
        fn kind(&self) -> &'static str {
            "test"
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
        fn keymap(&self) -> &KeyTrie {
            &self.keymap
        }
        fn command(&mut self, _: PaneCommand, _: usize) -> anyhow::Result<PaneOutcome> {
            Ok(PaneOutcome::None)
        }
        fn mouse(&mut self, _: &MouseEvent, _: Rect) {}
        fn update(&mut self, _: PaneId, _: &Editor, _: &mut Jobs) {}
        fn render(&mut self, _: Rect, _: &mut Buffer, _: &Editor, _: bool) {}
        fn cursor(&self, _: Rect) -> (Option<Position>, CursorKind) {
            (None, CursorKind::Hidden)
        }
    }

    fn add(manager: &mut PaneManager, dock: Dock) -> PaneId {
        manager.panes.insert(HostedPane {
            content: Box::new(TestPane {
                keymap: KeyTrie::Node(Default::default()),
            }),
            dock,
            width: 24,
            area: Rect::default(),
        })
    }

    #[test]
    fn multiple_pane_types_can_share_layout_and_stable_ids() {
        let mut manager = PaneManager::default();
        let left = add(&mut manager, Dock::Left);
        let right = add(&mut manager, Dock::Right);
        let mut focus = Some(right);
        assert_eq!(
            manager.layout(Rect::new(0, 1, 120, 30), &mut focus),
            Rect::new(24, 1, 72, 30)
        );
        assert_eq!(manager.panes[left].area, Rect::new(0, 1, 24, 30));
        assert_eq!(manager.panes[right].area, Rect::new(96, 1, 24, 30));
        assert!(manager.get_mut::<TestPane>(left).is_some());
        manager.panes.remove(left);
        let replacement = add(&mut manager, Dock::Left);
        assert_ne!(left, replacement);
        assert!(manager.get_mut::<TestPane>(left).is_none());
        manager.layout(Rect::new(0, 0, 10, 3), &mut focus);
        assert_eq!(focus, None);
        assert_eq!(manager.panes[right].area.width, 0);
    }

    #[test]
    fn directional_navigation_respects_adjacency_and_overlap() {
        let current = Rect::new(25, 1, 40, 10);
        assert_eq!(
            directional_distance(current, Rect::new(0, 1, 24, 20), Direction::Left),
            Some((1, 0))
        );
        assert_eq!(
            directional_distance(current, Rect::new(66, 1, 20, 10), Direction::Right),
            Some((1, 0))
        );
        assert_eq!(
            directional_distance(current, Rect::new(25, 11, 40, 10), Direction::Down),
            Some((0, 0))
        );
        assert_eq!(
            directional_distance(current, Rect::new(0, 12, 24, 10), Direction::Left),
            None
        );
        assert_eq!(directional_distance(current, current, Direction::Up), None);
    }
}
