//! Frontend pane hosting. Document splits keep their existing View/Document model;
//! auxiliary panes share focus, layout, command routing, and window navigation.
use std::any::Any;

use helix_core::Position;
use helix_view::{
    editor::{Action, PaneLayoutConfig, PanePosition},
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
    Callback(crate::compositor::Callback),
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
    /// Stateful tools can remain connected while their layout slot is hidden.
    fn retain_on_close(&self) -> bool {
        false
    }
    fn on_hide(&mut self) {}
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
    /// Outer group, allocated before the workspace regardless of opening order.
    Outer,
    /// Inner workspace group, surrounding the document split tree.
    Workspace,
    /// Render over the document region without replacing its buffers or splits.
    Center,
}

struct HostedPane {
    content: Box<dyn Pane>,
    dock: Dock,
    area: Rect,
    position: Option<PanePosition>,
    hidden: bool,
}

impl HostedPane {
    fn content_area(&self) -> Rect {
        pane_regions(self.area, self.position).0
    }
}

/// Reserve the edge facing the rest of the group, not the terminal's outside edge.
fn pane_regions(
    area: Rect,
    position: Option<PanePosition>,
) -> (Rect, Option<(Rect, &'static str)>) {
    if area.width == 0 || area.height == 0 {
        return (area, None);
    }
    match position {
        Some(PanePosition::Left) => (
            area.clip_right(1),
            Some((Rect::new(area.right() - 1, area.y, 1, area.height), "│")),
        ),
        Some(PanePosition::Right) => (area.clip_left(1), Some((area.with_width(1), "│"))),
        Some(PanePosition::Top) => (
            area.clip_bottom(1),
            Some((Rect::new(area.x, area.bottom() - 1, area.width, 1), "─")),
        ),
        Some(PanePosition::Bottom) => (area.clip_top(1), Some((area.with_height(1), "─"))),
        None => (area, None),
    }
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
    pub fn insert(&mut self, pane: impl Pane, dock: Dock, editor: &mut Editor) -> PaneId {
        let id = self.panes.insert(HostedPane {
            content: Box::new(pane),
            dock,
            area: Rect::default(),
            position: None,
            hidden: false,
        });
        self.focus(FocusTarget::Pane(id), editor);
        id
    }

    pub fn find(&self, kind: &str) -> Option<PaneId> {
        // Retained hidden panes still own their session and draft. Resolving
        // them also lets session pickers guard against discarding hidden work.
        self.panes
            .iter()
            .find_map(|(id, pane)| (pane.content.kind() == kind).then_some(id))
    }

    pub fn is_visible(&self, id: PaneId) -> bool {
        self.panes.get(id).is_some_and(|pane| !pane.hidden)
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
                    pane.hidden = false;
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
        if let Some(pane) = self
            .panes
            .get_mut(id)
            .filter(|pane| pane.content.retain_on_close())
        {
            pane.hidden = true;
            pane.area = Rect::default();
            pane.content.on_hide();
        } else {
            self.panes.remove(id);
        }
        if editor.focused_pane == Some(id) {
            self.focus_document(editor);
        }
    }

    /// Reserve docked regions, returning the region for the document split tree.
    pub fn layout(
        &mut self,
        mut area: Rect,
        focused: &mut Option<PaneId>,
        config: &PaneLayoutConfig,
    ) -> Rect {
        for (dock, position, size) in [
            (Dock::Outer, config.opencode_position, config.opencode_size),
            (
                Dock::Workspace,
                config.file_explorer_position,
                config.file_explorer_size,
            ),
        ] {
            for (id, pane) in &mut self.panes {
                if pane.dock != dock || pane.hidden {
                    continue;
                }
                (pane.area, area) = split_pane(area, position, size);
                pane.position = Some(position);
                if (pane.area.width == 0 || pane.area.height == 0) && *focused == Some(id) {
                    *focused = None;
                }
            }
        }
        self.layout_center(area, focused);
        area
    }

    /// Place review content below document-only chrome such as the bufferline.
    pub fn layout_center(&mut self, area: Rect, focused: &mut Option<PaneId>) {
        for (id, pane) in &mut self.panes {
            if pane.dock == Dock::Center && !pane.hidden {
                pane.area = area;
                if (area.width == 0 || area.height == 0) && *focused == Some(id) {
                    *focused = None;
                }
            }
        }
    }

    pub fn render(&mut self, surface: &mut Buffer, editor: &Editor, jobs: &mut Jobs) {
        for (id, pane) in &mut self.panes {
            if pane.hidden {
                continue;
            }
            pane.content.update(id, editor, jobs);
            // Document focus must reveal the document, never allow edits behind
            // a visually covering review pane.
            if pane.dock == Dock::Center && editor.focused_pane.is_none() {
                continue;
            }
            if pane.area.width > 0 && pane.area.height > 0 {
                let (content, separator) = pane_regions(pane.area, pane.position);
                pane.content
                    .render(content, surface, editor, editor.focused_pane == Some(id));
                if let Some((edge, symbol)) = separator {
                    for y in edge.y..edge.bottom() {
                        for x in edge.x..edge.right() {
                            surface.set_string(x, y, symbol, editor.theme.get("ui.window"));
                        }
                    }
                }
            }
        }
    }

    pub fn cursor(&self, editor: &Editor) -> Option<(Option<Position>, CursorKind)> {
        let pane = self.panes.get(editor.focused_pane?)?;
        (!pane.hidden).then(|| pane.content.cursor(pane.content_area()))
    }

    pub fn refresh(&mut self) {
        for pane in self.panes.values_mut() {
            pane.content.refresh();
        }
    }

    pub fn mouse(&mut self, event: &MouseEvent, editor: &mut Editor) -> bool {
        let target = self.panes.iter().find_map(|(id, pane)| {
            (!pane.hidden
                && (pane.dock != Dock::Center || editor.focused_pane.is_some())
                && pane.area.width > 0
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
            let content = pane.content_area();
            if event.column >= content.x
                && event.column < content.right()
                && event.row >= content.y
                && event.row < content.bottom()
            {
                pane.content.mouse(event, content);
            }
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
        let Some(pane) = self.panes.get_mut(id) else {
            editor.focused_pane = None;
            return None;
        };
        match pane.content.command(command, count) {
            Ok(PaneOutcome::None) => (),
            Ok(PaneOutcome::Callback(callback)) => return Some(callback),
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

    /// Whether a review pane covers the document region. Document focus always
    /// reveals the documents instead.
    pub fn covers_documents(&self, editor: &Editor) -> bool {
        editor.focused_pane.is_some()
            && self
                .panes
                .values()
                .any(|pane| pane.dock == Dock::Center && !pane.hidden)
    }

    fn targets(&self, editor: &Editor) -> Vec<(FocusTarget, Rect)> {
        let center_visible = self.covers_documents(editor);
        let mut targets: Vec<_> = editor
            .tree
            .views()
            .filter(|_| !center_visible)
            .map(|(view, _)| (FocusTarget::Document(view.id), view.area))
            .collect();
        targets.extend(
            self.panes
                .iter()
                .filter(|(_, pane)| {
                    pane.area.width > 0
                        && pane.area.height > 0
                        && !pane.hidden
                        && (pane.dock != Dock::Center || center_visible)
                })
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
        if self.panes.values().all(|pane| pane.hidden) {
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
        if self.panes.values().all(|pane| pane.hidden) {
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

/// Keep a usable remainder at each level. If there is not enough room for both
/// regions, hide the auxiliary pane rather than covering or starving documents.
fn split_pane(area: Rect, position: PanePosition, percentage: u8) -> (Rect, Rect) {
    let horizontal = matches!(position, PanePosition::Left | PanePosition::Right);
    let (available, pane_minimum, remainder_minimum, cross) = if horizontal {
        (area.width, 8, 20, area.height)
    } else {
        (area.height, 4, 4, area.width)
    };
    let size = (u32::from(available) * u32::from(percentage.clamp(1, 99)) / 100) as u16;
    if size < pane_minimum || available.saturating_sub(size) < remainder_minimum || cross == 0 {
        return (Rect::new(area.x, area.y, 0, 0), area);
    }
    match position {
        PanePosition::Left => (area.with_width(size), area.clip_left(size)),
        PanePosition::Right => (
            Rect::new(area.right() - size, area.y, size, area.height),
            area.with_width(area.width - size),
        ),
        PanePosition::Top => (
            Rect::new(area.x, area.y, area.width, size),
            Rect::new(area.x, area.y + size, area.width, area.height - size),
        ),
        PanePosition::Bottom => (
            Rect::new(area.x, area.bottom() - size, area.width, size),
            Rect::new(area.x, area.y, area.width, area.height - size),
        ),
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
            area: Rect::default(),
            position: None,
            hidden: false,
        })
    }

    #[test]
    fn separators_face_the_neighbor_for_each_position() {
        let area = Rect::new(4, 3, 30, 12);
        for (position, content, edge, symbol) in [
            (
                PanePosition::Left,
                Rect::new(4, 3, 29, 12),
                Rect::new(33, 3, 1, 12),
                "│",
            ),
            (
                PanePosition::Right,
                Rect::new(5, 3, 29, 12),
                Rect::new(4, 3, 1, 12),
                "│",
            ),
            (
                PanePosition::Top,
                Rect::new(4, 3, 30, 11),
                Rect::new(4, 14, 30, 1),
                "─",
            ),
            (
                PanePosition::Bottom,
                Rect::new(4, 4, 30, 11),
                Rect::new(4, 3, 30, 1),
                "─",
            ),
        ] {
            assert_eq!(
                pane_regions(area, Some(position)),
                (content, Some((edge, symbol)))
            );
            assert_eq!(
                pane_regions(Rect::default(), Some(position)),
                (Rect::default(), None)
            );
        }
        assert_eq!(pane_regions(area, None), (area, None));
    }

    #[test]
    fn multiple_pane_types_can_share_layout_and_stable_ids() {
        let mut manager = PaneManager::default();
        let left = add(&mut manager, Dock::Outer);
        let right = add(&mut manager, Dock::Workspace);
        let mut focus = Some(right);
        assert_eq!(
            manager.layout(
                Rect::new(0, 1, 200, 30),
                &mut focus,
                &PaneLayoutConfig::default()
            ),
            Rect::new(110, 1, 90, 30)
        );
        assert_eq!(manager.panes[left].area, Rect::new(0, 1, 80, 30));
        assert_eq!(manager.panes[right].area, Rect::new(80, 1, 30, 30));
        assert!(manager.get_mut::<TestPane>(left).is_some());
        manager.panes.remove(left);
        let replacement = add(&mut manager, Dock::Outer);
        assert_ne!(left, replacement);
        assert!(manager.get_mut::<TestPane>(left).is_none());
        manager.layout(
            Rect::new(0, 0, 10, 3),
            &mut focus,
            &PaneLayoutConfig::default(),
        );
        assert_eq!(focus, None);
        assert_eq!(manager.panes[right].area.width, 0);
    }

    #[test]
    fn central_review_uses_the_document_region_without_consuming_its_geometry() {
        let mut manager = PaneManager::default();
        let center = add(&mut manager, Dock::Center);
        let left = add(&mut manager, Dock::Outer);
        let right = add(&mut manager, Dock::Workspace);
        let mut focus = Some(center);
        let document = manager.layout(
            Rect::new(0, 1, 200, 30),
            &mut focus,
            &PaneLayoutConfig::default(),
        );
        assert_eq!(document, Rect::new(110, 1, 90, 30));
        assert_eq!(manager.panes[center].area, document);
        assert_eq!(manager.panes[left].area.width, 80);
        assert_eq!(manager.panes[right].area.width, 30);
        assert_eq!(focus, Some(center));
        manager.layout_center(document.clip_top(1), &mut focus);
        assert_eq!(manager.panes[center].area, document.clip_top(1));
        assert_eq!(manager.panes[left].area.y, 1);
        assert_eq!(manager.panes[right].area.y, 1);
        manager.panes.remove(center);
        assert_eq!(
            manager.layout(
                Rect::new(0, 1, 200, 30),
                &mut None,
                &PaneLayoutConfig::default()
            ),
            document
        );
    }

    #[test]
    fn hidden_panes_relinquish_space_without_losing_their_identity() {
        let mut manager = PaneManager::default();
        let chat = add(&mut manager, Dock::Outer);
        let area = Rect::new(0, 0, 120, 30);
        let config = PaneLayoutConfig::default();
        let shown = manager.layout(area, &mut None, &config);
        manager.panes[chat].hidden = true;
        manager.panes[chat].area = Rect::default();
        assert_eq!(manager.layout(area, &mut None, &config), area);
        assert!(manager.get_mut::<TestPane>(chat).is_some());
        manager.panes[chat].hidden = false;
        assert_eq!(manager.layout(area, &mut None, &config), shown);
    }

    #[test]
    fn requested_proportions_fit_a_standard_terminal() {
        let mut manager = PaneManager::default();
        let explorer = add(&mut manager, Dock::Workspace);
        let chat = add(&mut manager, Dock::Outer);
        let config = PaneLayoutConfig::default();
        assert_eq!(
            manager.layout(Rect::new(0, 0, 120, 30), &mut None, &config),
            Rect::new(66, 0, 54, 30)
        );
        assert_eq!(manager.panes[chat].area, Rect::new(0, 0, 48, 30));
        assert_eq!(manager.panes[explorer].area, Rect::new(48, 0, 18, 30));
    }

    #[test]
    fn nested_layout_is_independent_of_open_order_and_reclaims_closed_panes() {
        let area = Rect::new(3, 2, 200, 40);
        let config = PaneLayoutConfig::default();
        for reversed in [false, true] {
            let mut manager = PaneManager::default();
            let (chat, explorer) = if reversed {
                let explorer = add(&mut manager, Dock::Workspace);
                (add(&mut manager, Dock::Outer), explorer)
            } else {
                (
                    add(&mut manager, Dock::Outer),
                    add(&mut manager, Dock::Workspace),
                )
            };
            assert_eq!(
                manager.layout(area, &mut None, &config),
                Rect::new(113, 2, 90, 40)
            );
            assert_eq!(manager.panes[chat].area, Rect::new(3, 2, 80, 40));
            assert_eq!(manager.panes[explorer].area, Rect::new(83, 2, 30, 40));
            manager.panes.remove(chat);
            assert_eq!(
                manager.layout(area, &mut None, &config),
                Rect::new(53, 2, 150, 40)
            );
            manager.panes.remove(explorer);
            assert_eq!(manager.layout(area, &mut None, &config), area);
            let chat = add(&mut manager, Dock::Outer);
            assert_eq!(
                manager.layout(area, &mut None, &config),
                Rect::new(83, 2, 120, 40)
            );
            assert_eq!(manager.panes[chat].area.width, 80);
        }
    }

    #[test]
    fn placement_and_sizes_update_without_reopening_panes() {
        let mut manager = PaneManager::default();
        let explorer = add(&mut manager, Dock::Workspace);
        let chat = add(&mut manager, Dock::Outer);
        let mut config = PaneLayoutConfig {
            opencode_position: PanePosition::Right,
            opencode_size: 30,
            file_explorer_position: PanePosition::Right,
            file_explorer_size: 50,
        };
        let area = Rect::new(5, 3, 200, 40);
        assert_eq!(
            manager.layout(area, &mut None, &config),
            Rect::new(5, 3, 70, 40)
        );
        assert_eq!(manager.panes[chat].area, Rect::new(145, 3, 60, 40));
        assert_eq!(manager.panes[explorer].area, Rect::new(75, 3, 70, 40));
        config.opencode_position = PanePosition::Bottom;
        config.opencode_size = 40;
        config.file_explorer_position = PanePosition::Top;
        config.file_explorer_size = 25;
        assert_eq!(
            manager.layout(area, &mut None, &config),
            Rect::new(5, 9, 200, 18)
        );
        assert_eq!(manager.panes[chat].area, Rect::new(5, 27, 200, 16));
        assert_eq!(manager.panes[explorer].area, Rect::new(5, 3, 200, 6));
        assert_eq!(
            manager.layout(Rect::new(5, 3, 100, 20), &mut None, &config),
            Rect::new(5, 3, 100, 12)
        );
        // The inner pane is hidden when its three rows cannot render useful content.
        assert_eq!(manager.panes[explorer].area.width, 0);
    }

    #[test]
    fn small_and_zero_areas_hide_panes_and_restore_document_focus() {
        for position in [
            PanePosition::Left,
            PanePosition::Right,
            PanePosition::Top,
            PanePosition::Bottom,
        ] {
            for width in 0..65 {
                for height in 0..12 {
                    let mut manager = PaneManager::default();
                    let chat = add(&mut manager, Dock::Outer);
                    add(&mut manager, Dock::Workspace);
                    let config = PaneLayoutConfig {
                        opencode_position: position,
                        file_explorer_position: position,
                        ..PaneLayoutConfig::default()
                    };
                    let area = Rect::new(3, 2, width, height);
                    let mut focus = Some(chat);
                    let document = manager.layout(area, &mut focus, &config);
                    assert!(document.x >= area.x && document.right() <= area.right());
                    assert!(document.y >= area.y && document.bottom() <= area.bottom());
                    for pane in manager.panes.values() {
                        assert!(pane.area.right() <= area.right());
                        assert!(pane.area.bottom() <= area.bottom());
                    }
                    if manager.panes[chat].area.width == 0 || manager.panes[chat].area.height == 0 {
                        assert_eq!(focus, None);
                    }
                }
            }
        }
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
