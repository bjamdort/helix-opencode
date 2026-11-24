use std::{path::PathBuf, sync::Arc};
mod interactions;
pub(crate) mod models;
pub(super) mod references;
pub(crate) mod sessions;
pub(crate) mod snippets;
mod transcript;

use arc_swap::ArcSwap;
use helix_core::{
    hashmap, syntax,
    unicode::{segmentation::UnicodeSegmentation, width::UnicodeWidthStr},
    Position,
};
use helix_view::theme::Modifier;
use helix_view::{
    graphics::{CursorKind, Rect, Style, UnderlineStyle},
    input::{MouseEvent, MouseEventKind},
    Editor, PaneId, Theme,
};
use parking_lot::Mutex;
use tui::{
    buffer::Buffer,
    text::{Span, Spans},
};

use super::{
    markdown::chat_style,
    pane::{Pane, PaneOutcome},
};
use crate::{
    commands::pane::PaneCommand,
    job::Jobs,
    keymap::{macros::keymap, KeyTrie},
    opencode::{self, Connection, State},
};
use transcript::{ChatRow, Palette};

pub(super) const ID: &str = "opencode";

pub(super) struct OpenCode {
    pub session_id: String,
    directory: PathBuf,
    connection: Connection,
    draft: Arc<Mutex<String>>,
    input: super::prompt::SharedPromptAnchor,
    panel: Arc<Mutex<Rect>>,
    keys: KeyTrie,
    offset: usize,
    follow: bool,
    viewport_height: usize,
    expanded: bool,
    references: Vec<(references::Reference, usize)>,
    messages: Vec<(String, usize)>,
    selected_reference: Option<references::Reference>,
    selected_message: Option<String>,
    snippets: Vec<opencode::snippets::Snippet>,
}

impl OpenCode {
    pub(super) fn session_directory(&self) -> PathBuf {
        self.connection.state.lock().session.as_ref().map_or_else(
            || self.directory.clone(),
            |session| session.location.directory.clone(),
        )
    }
    pub(super) fn changes_handle(&self) -> anyhow::Result<opencode::changes::ChangesHandle> {
        self.connection.changes_handle()
    }

    pub(super) fn matches_changes_handle(&self, handle: &opencode::changes::ChangesHandle) -> bool {
        self.connection.matches_changes_handle(handle)
    }
    pub(super) fn model_handle(&self) -> anyhow::Result<opencode::models::ModelHandle> {
        self.connection.model_handle()
    }

    pub(super) fn matches_model_handle(&self, handle: &opencode::models::ModelHandle) -> bool {
        self.connection.matches_model_handle(handle)
    }
    pub fn new(directory: PathBuf, session: Option<String>) -> anyhow::Result<Self> {
        if session
            .as_deref()
            .is_some_and(|id| !opencode::valid_id(id, "ses"))
        {
            anyhow::bail!("Invalid OpenCode session ID");
        }
        let connection = Connection::new(directory.clone(), session);
        Ok(Self {
            session_id: connection.id.clone(),
            directory,
            connection,
            draft: Arc::new(Mutex::new(String::new())),
            input: Arc::new(Mutex::new(super::prompt::PromptAnchor::default())),
            panel: Arc::new(Mutex::new(Rect::default())),
            keys: keymap!({ "OpenCode chat"
                "i" => pane_compose,
                "ret" => pane_activate,
                "]" => pane_next_reference,
                "[" => pane_previous_reference,
                "o" => pane_open_reference,
                "f" => pane_references,
                "J" => pane_next_message,
                "K" => pane_previous_message,
                "c" | "C-c" => pane_interrupt,
                "n" => pane_new,
                "r" => pane_refresh,
                "t" => pane_toggle_details,
                "p" => pane_requests,
                "D" => pane_review,
                "q" => pane_close,
                "esc" | "tab" => focus_document_pane,
                "home" => goto_file_start,
                "end" | "G" => goto_last_line,
            }),
            offset: 0,
            follow: true,
            viewport_height: 1,
            expanded: false,
            references: Vec::new(),
            messages: Vec::new(),
            selected_reference: None,
            selected_message: None,
            snippets: Vec::new(),
        })
    }

    fn scroll(&mut self, command: PaneCommand, count: usize) {
        self.selected_reference = None;
        self.selected_message = None;
        match command {
            PaneCommand::Up => {
                self.follow = false;
                self.offset = self.offset.saturating_sub(count);
            }
            PaneCommand::Down => self.offset = self.offset.saturating_add(count),
            PaneCommand::PageUp => {
                self.follow = false;
                self.offset = self
                    .offset
                    .saturating_sub(self.viewport_height.saturating_mul(count));
            }
            PaneCommand::PageDown => {
                self.offset = self
                    .offset
                    .saturating_add(self.viewport_height.saturating_mul(count))
            }
            PaneCommand::First => {
                self.follow = false;
                self.offset = 0;
            }
            PaneCommand::Last => self.follow = true,
            _ => (),
        }
    }

    fn reference_list(&self) -> PaneOutcome {
        let state = self.connection.state.lock();
        let directory = state
            .session
            .as_ref()
            .map_or(self.directory.as_path(), |session| {
                session.location.directory.as_path()
            });
        let references = state
            .messages
            .iter()
            .flat_map(|message| references::for_message(message, directory))
            .collect();
        PaneOutcome::Overlay(Box::new(references::ReferenceList::new(
            references,
            self.panel.clone(),
        )))
    }

    fn navigate_reference(&mut self, forward: bool, count: usize) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.references.is_empty(),
            "No file references in the loaded conversation"
        );
        let selected = self.selected_reference.as_ref().and_then(|selected| {
            self.references
                .iter()
                .position(|(reference, _)| reference == selected)
        });
        let index = navigation_index(
            &self
                .references
                .iter()
                .map(|(_, row)| *row)
                .collect::<Vec<_>>(),
            selected,
            self.offset,
            forward,
            count,
        );
        let (reference, row) = &self.references[index];
        self.selected_reference = Some(reference.clone());
        self.selected_message = None;
        self.offset = row.saturating_sub(1);
        self.follow = false;
        Ok(())
    }

    fn navigate_message(&mut self, forward: bool, count: usize) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.messages.is_empty(),
            "No messages in the loaded conversation"
        );
        let selected = self
            .selected_message
            .as_ref()
            .and_then(|selected| self.messages.iter().position(|(id, _)| id == selected));
        let index = navigation_index(
            &self
                .messages
                .iter()
                .map(|(_, row)| *row)
                .collect::<Vec<_>>(),
            selected,
            self.offset,
            forward,
            count,
        );
        let (id, row) = &self.messages[index];
        self.selected_message = Some(id.clone());
        self.selected_reference = None;
        self.offset = *row;
        self.follow = false;
        Ok(())
    }
}

fn navigation_index(
    rows: &[usize],
    selected: Option<usize>,
    offset: usize,
    forward: bool,
    count: usize,
) -> usize {
    let count = count.max(1);
    if let Some(selected) = selected {
        if forward {
            selected.saturating_add(count).min(rows.len() - 1)
        } else {
            selected.saturating_sub(count)
        }
    } else if forward {
        rows.iter()
            .position(|row| *row >= offset)
            .unwrap_or(rows.len() - 1)
            .saturating_add(count - 1)
            .min(rows.len() - 1)
    } else {
        rows.iter()
            .rposition(|row| *row <= offset)
            .unwrap_or(0)
            .saturating_sub(count - 1)
    }
}

impl Pane for OpenCode {
    fn kind(&self) -> &'static str {
        ID
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn keymap(&self) -> &KeyTrie {
        &self.keys
    }
    fn retain_on_close(&self) -> bool {
        true
    }
    fn on_hide(&mut self) {
        *self.panel.lock() = Rect::default();
        self.input.lock().area = Rect::default();
    }

    fn command(&mut self, command: PaneCommand, count: usize) -> anyhow::Result<PaneOutcome> {
        match command {
            PaneCommand::Activate if self.selected_reference.is_some() => {
                return self.selected_reference.as_ref().unwrap().outcome()
            }
            PaneCommand::NextReference => self.navigate_reference(true, count)?,
            PaneCommand::PreviousReference => self.navigate_reference(false, count)?,
            PaneCommand::NextMessage => self.navigate_message(true, count)?,
            PaneCommand::PreviousMessage => self.navigate_message(false, count)?,
            PaneCommand::References => return Ok(self.reference_list()),
            PaneCommand::OpenReference => {
                return match &self.selected_reference {
                    Some(reference) => reference.outcome(),
                    None => Ok(self.reference_list()),
                }
            }
            PaneCommand::Compose | PaneCommand::Activate => {
                self.selected_reference = None;
                self.selected_message = None;
                return Ok(PaneOutcome::Compose {
                    title: "OpenCode > ",
                    draft: self.draft.clone(),
                    anchor: Some(self.input.clone()),
                });
            }
            PaneCommand::Interrupt => self.connection.interrupt()?,
            PaneCommand::Refresh => self.connection.reconnect(),
            PaneCommand::Requests => {
                return Ok(PaneOutcome::Overlay(Box::new(
                    interactions::Interaction::requests(
                        self.connection.interactions(),
                        self.panel.clone(),
                    ),
                )))
            }
            PaneCommand::Review => {
                let handle = self.connection.interactions();
                handle.review()?;
                return Ok(PaneOutcome::Overlay(Box::new(
                    interactions::Interaction::review(handle, self.panel.clone()),
                )));
            }
            PaneCommand::ToggleDetails => self.expanded = !self.expanded,
            PaneCommand::Right => self.expanded = true,
            PaneCommand::Left => self.expanded = false,
            PaneCommand::New => {
                self.connection = Connection::new(self.directory.clone(), None);
                self.session_id = self.connection.id.clone();
                self.draft.lock().clear();
                self.snippets.clear();
                self.offset = 0;
                self.follow = true;
                self.references.clear();
                self.messages.clear();
                self.selected_reference = None;
                self.selected_message = None;
            }
            PaneCommand::Close => return Ok(PaneOutcome::Close),
            PaneCommand::FocusDocument => return Ok(PaneOutcome::FocusDocument),
            PaneCommand::OpenHorizontalSplit | PaneCommand::OpenVerticalSplit => {
                anyhow::bail!("This action requires a file explorer pane");
            }
            _ => self.scroll(command, count),
        }
        Ok(PaneOutcome::None)
    }

    fn submit(&mut self, text: String) -> anyhow::Result<()> {
        if self.snippets.is_empty() {
            self.connection.send(text)?;
        } else {
            self.connection.send_snippets(text, self.snippets.clone())?;
            self.snippets.clear();
        }
        self.follow = true;
        self.selected_reference = None;
        self.selected_message = None;
        Ok(())
    }

    fn mouse(&mut self, event: &MouseEvent, _area: Rect) {
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll(PaneCommand::Up, 3),
            MouseEventKind::ScrollDown => self.scroll(PaneCommand::Down, 3),
            _ => (),
        }
    }

    fn update(&mut self, _id: PaneId, _editor: &Editor, _jobs: &mut Jobs) {}

    fn render(&mut self, area: Rect, surface: &mut Buffer, editor: &Editor, focused: bool) {
        *self.panel.lock() = area;
        let palette = Palette::new(&editor.theme);
        let editing = self.input.lock().active;
        let state = self.connection.state.lock();
        let draft = self.draft.lock();
        let view = ChatView {
            state: &state,
            draft: &draft,
            directory: &self.directory,
            palette: &palette,
            theme: &editor.theme,
            loader: &editor.syn_loader,
            expanded: self.expanded,
            focused,
            editing,
            snippets: &self.snippets,
            selecting: self.selected_reference.is_some(),
        };
        let layout = view.layout(area);
        self.input.lock().area = layout.input_area();
        self.viewport_height = layout.body.height.max(1) as usize;
        let mut lines = view.transcript(layout.body.width as usize);
        self.references.clear();
        self.messages.clear();
        let mut seen = std::collections::HashSet::new();
        for (index, row) in lines.iter_mut().enumerate() {
            if let Some(id) = &row.message {
                if self
                    .messages
                    .last()
                    .is_none_or(|(previous, _)| previous != id)
                {
                    self.messages.push((id.clone(), index));
                }
            }
            for (span_index, reference) in &row.links {
                if seen.insert(reference.clone()) {
                    self.references.push((reference.clone(), index));
                }
                if Some(reference) == self.selected_reference.as_ref() {
                    if let Some(span) = row.spans.0.get_mut(*span_index) {
                        span.style = span
                            .style
                            .patch(editor.theme.get("ui.selection"))
                            .add_modifier(Modifier::BOLD)
                            .underline_style(UnderlineStyle::Line);
                    }
                }
            }
        }
        if self.selected_reference.as_ref().is_some_and(|selected| {
            !self
                .references
                .iter()
                .any(|(reference, _)| reference == selected)
        }) {
            self.selected_reference = None;
        }
        if let Some((_, row)) = self.selected_reference.as_ref().and_then(|selected| {
            self.references
                .iter()
                .find(|(reference, _)| reference == selected)
        }) {
            if *row < self.offset || *row >= self.offset.saturating_add(self.viewport_height) {
                self.offset = row.saturating_sub(1);
            }
        }
        let last = lines.len().saturating_sub(layout.body.height as usize);
        if self.follow {
            self.offset = last;
        }
        self.offset = self.offset.min(last);
        if self.offset == last
            && layout.body.height > 0
            && self.selected_reference.is_none()
            && self.selected_message.is_none()
        {
            self.follow = true;
        }
        view.render(area, surface, &layout, &lines, self.offset, self.follow);
    }

    fn cursor(&self, area: Rect) -> (Option<Position>, CursorKind) {
        (
            Some(Position::new(area.y as usize, area.x as usize)),
            CursorKind::Hidden,
        )
    }
}

/// The OpenCode TUI session layout: a transcript that sticks to the bottom, a
/// reserved "Jump to latest" row, an optional request dock, and the prompt
/// with its metadata row and footer. Small panes drop padding first so
/// rectangles never overlap or underflow.
struct ChatLayout {
    body: Rect,
    jump: Rect,
    dock: Rect,
    composer: Rect,
    chips: Rect,
    input: Rect,
    meta: Rect,
    edge: Rect,
    footer: Rect,
}

impl ChatLayout {
    fn new(area: Rect, input_rows: u16, chip_rows: u16, dock_rows: u16) -> Self {
        let inner = area.clip_left(1).clip_right(1);
        let footer_height = u16::from(inner.height > 0);
        let rest = inner.clip_bottom(footer_height);
        let input_rows = input_rows.max(1);
        let roomy = pad_rows(input_rows, chip_rows) + 2 + 4 <= rest.height;
        let (pad, chips, edge, input_rows) = if roomy {
            (1, chip_rows, 1, input_rows)
        } else {
            (0, 0, 0, 1)
        };
        let composer_height = (pad * 2 + chips + input_rows + 1 + edge).min(rest.height);
        let composer = Rect::new(
            rest.x,
            rest.bottom() - composer_height,
            rest.width,
            composer_height,
        );
        let above = rest.clip_bottom(composer_height);
        let dock_height = if dock_rows > 0 && above.height >= dock_rows + 1 + 4 {
            dock_rows + 1
        } else {
            0
        };
        let dock = Rect::new(
            above.x,
            above.bottom() - dock_height,
            above.width,
            dock_height.saturating_sub(1),
        );
        let above = above.clip_bottom(dock_height);
        let jump_height = u16::from(above.height >= 3);
        let jump = Rect::new(
            above.x,
            above.bottom() - jump_height,
            above.width,
            jump_height,
        );
        let body = above.clip_bottom(jump_height);
        let content = composer.clip_left(3).clip_right(2);
        let row = |offset: u16, height: u16| {
            if offset >= composer.height {
                Rect::new(content.x, composer.bottom(), content.width, 0)
            } else {
                Rect::new(
                    content.x,
                    composer.y + offset,
                    content.width,
                    height.min(composer.height - offset),
                )
            }
        };
        let chips_area = row(pad, chips);
        let input = row(pad + chips, input_rows);
        let meta = row(pad * 2 + chips + input_rows, 1);
        let edge = if edge > 0 {
            Rect::new(composer.x, composer.bottom() - 1, composer.width, 1)
        } else {
            Rect::new(composer.x, composer.bottom(), composer.width, 0)
        };
        Self {
            body,
            jump,
            dock,
            composer,
            chips: chips_area,
            input,
            meta,
            edge,
            footer: Rect::new(
                inner.x,
                inner.bottom() - footer_height,
                inner.width,
                footer_height,
            ),
        }
    }

    fn input_area(&self) -> Rect {
        if self.input.width < 3 || self.input.height == 0 {
            Rect::default()
        } else {
            self.input.with_height(1)
        }
    }
}

fn pad_rows(input_rows: u16, chip_rows: u16) -> u16 {
    2 + chip_rows + input_rows + 1
}

struct ChatView<'a> {
    state: &'a State,
    draft: &'a str,
    directory: &'a std::path::Path,
    palette: &'a Palette,
    theme: &'a Theme,
    loader: &'a Arc<ArcSwap<syntax::Loader>>,
    expanded: bool,
    focused: bool,
    editing: bool,
    snippets: &'a [opencode::snippets::Snippet],
    selecting: bool,
}

/// A pending permission or question, summarized above the prompt.
struct Request {
    question: bool,
    detail: String,
}

impl ChatView<'_> {
    fn session_directory(&self) -> &std::path::Path {
        self.state
            .session
            .as_ref()
            .map_or(self.directory, |session| {
                session.location.directory.as_path()
            })
    }

    fn transcript(&self, width: usize) -> Vec<ChatRow> {
        transcript::build(
            transcript::TranscriptInput {
                state: self.state,
                palette: self.palette,
                theme: self.theme,
                loader: self.loader,
                directory: self.session_directory(),
                expanded: self.expanded,
            },
            width,
        )
    }

    fn input_width(area: Rect) -> usize {
        area.width.saturating_sub(2 + 5) as usize
    }

    /// The idle preview of the draft, or a placeholder.
    fn draft_rows(&self, width: usize) -> Vec<ChatRow> {
        let mut rows = Vec::new();
        let (text, style) = if !self.draft.is_empty() {
            (self.draft, self.palette.text)
        } else if self.state.connected {
            ("Ask anything…", self.palette.muted)
        } else if self.state.status.starts_with("Connecting") {
            ("Connecting to OpenCode…", self.palette.muted)
        } else {
            ("Press r to reconnect", self.palette.muted)
        };
        for line in text.replace('\t', "    ").split('\n') {
            transcript::wrap(
                &mut rows,
                &Spans::from(Span::styled(line, style)),
                &[],
                width.max(1),
                0,
            );
        }
        rows
    }

    fn requests(&self) -> Vec<Request> {
        self.state
            .permissions
            .values()
            .map(|permission| Request {
                question: false,
                detail: match permission.resources.first() {
                    Some(resource) => format!("{} {resource}", permission.action),
                    None => permission.action.clone(),
                },
            })
            .chain(self.state.forms.values().map(|form| Request {
                question: true,
                detail: form.title.clone(),
            }))
            .collect()
    }

    fn layout(&self, area: Rect) -> ChatLayout {
        let input_rows = if self.editing {
            1
        } else {
            self.draft_rows(Self::input_width(area)).len().clamp(1, 3) as u16
        };
        let requests = self.requests().len().min(2) as u16;
        let dock_rows = if requests == 0 { 0 } else { 4 + requests };
        ChatLayout::new(
            area,
            input_rows,
            u16::from(!self.snippets.is_empty()),
            dock_rows,
        )
    }

    fn render(
        &self,
        area: Rect,
        surface: &mut Buffer,
        layout: &ChatLayout,
        lines: &[ChatRow],
        offset: usize,
        follow: bool,
    ) {
        let palette = self.palette;
        fill(area, surface, palette.page);
        if area.width < 6 || area.height == 0 {
            return;
        }
        if lines.is_empty() && layout.body.height > 3 {
            self.welcome(layout.body, surface);
        } else {
            for (index, row) in lines
                .iter()
                .skip(offset)
                .take(layout.body.height as usize)
                .enumerate()
            {
                row.render(
                    Rect::new(
                        layout.body.x,
                        layout.body.y + index as u16,
                        layout.body.width,
                        1,
                    ),
                    surface,
                );
            }
        }
        if !follow {
            let label = "Jump to latest ↓";
            let width = (label.width() as u16).min(layout.jump.width);
            write(
                Rect::new(
                    layout.jump.right() - width,
                    layout.jump.y,
                    width,
                    layout.jump.height,
                ),
                surface,
                label,
                palette.muted,
            );
        }
        self.dock(layout.dock, surface);
        self.composer(layout, surface);
        self.footer(layout.footer, surface);
    }

    fn welcome(&self, body: Rect, surface: &mut Buffer) {
        let palette = self.palette;
        let y = body.y + body.height.saturating_sub(4) / 2;
        let area = body.clip_top(y - body.y).clip_left(transcript::INDENT);
        write(
            area.with_height(1),
            surface,
            "What would you like to work on?",
            palette.text.add_modifier(Modifier::BOLD),
        );
        if area.height >= 3 {
            write(
                area.clip_top(2).with_height(1),
                surface,
                "Ask about code, plan a change,",
                palette.muted,
            );
        }
        if area.height >= 4 {
            write(
                area.clip_top(3).with_height(1),
                surface,
                "or work through a problem.",
                palette.muted,
            );
        }
    }

    /// Pending requests replace the TUI's prompt; here they sit above it,
    /// because answering happens in the request list.
    fn dock(&self, area: Rect, surface: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let palette = self.palette;
        let requests = self.requests();
        surface.set_style(area, palette.panel);
        for y in area.y..area.bottom() {
            surface.set_stringn(area.x, y, "┃", 1, palette.accent);
        }
        let content = area.clip_left(3).clip_right(2).clip_top(1);
        let title = if requests.iter().all(|request| request.question) {
            "Question"
        } else {
            "Permission required"
        };
        draw_spans(
            content.with_height(1),
            surface,
            &[
                Span::styled("△ ", palette.warning),
                Span::styled(title, palette.text),
            ],
        );
        for (index, request) in requests.iter().take(2).enumerate() {
            let icon = if request.question { "?" } else { "→" };
            write(
                content.clip_top(1 + index as u16).with_height(1),
                surface,
                &format!("{icon} {}", request.detail),
                palette.muted,
            );
        }
        let shown = requests.len().min(2) as u16;
        let mut hints = vec![
            Span::styled("p", palette.text),
            Span::styled(" respond", palette.muted),
        ];
        if requests.len() > 2 {
            hints.push(Span::styled(
                format!("  +{} more", requests.len() - 2),
                palette.muted,
            ));
        }
        draw_spans(content.clip_top(1 + shown).with_height(1), surface, &hints);
    }

    fn composer(&self, layout: &ChatLayout, surface: &mut Buffer) {
        let palette = self.palette;
        let composer = layout.composer;
        if composer.height == 0 {
            return;
        }
        let card = composer.clip_bottom(layout.edge.height);
        surface.set_style(card, palette.prompt);
        let bar = if self.focused {
            palette.agent
        } else {
            palette.border
        };
        for y in card.y..card.bottom() {
            surface.set_stringn(card.x, y, "┃", 1, bar);
        }
        if layout.edge.height > 0 {
            // A half-height lower edge, as the TUI draws it.
            let edge = layout.edge;
            surface.set_stringn(edge.x, edge.y, "╹", 1, bar);
            let lower = Style {
                fg: palette.prompt.bg,
                ..palette.page
            };
            let width = edge.width.saturating_sub(1) as usize;
            surface.set_stringn(edge.x + 1, edge.y, &"▀".repeat(width), width, lower);
        }
        if layout.chips.height > 0 {
            if let Some(first) = self.snippets.first() {
                let comments = self
                    .snippets
                    .iter()
                    .filter(|snippet| !snippet.comment.is_empty())
                    .count();
                let mut spans = vec![
                    Span::styled(
                        " file ",
                        Style {
                            fg: palette.page.bg,
                            bg: palette.accent.fg,
                            ..Style::default()
                        }
                        .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {} ", first.label()),
                        palette.panel.patch(Style {
                            fg: palette.muted.fg,
                            ..Style::default()
                        }),
                    ),
                ];
                if self.snippets.len() > 1 {
                    spans.push(Span::styled(
                        format!(" +{}", self.snippets.len() - 1),
                        palette.muted,
                    ));
                }
                if comments > 0 {
                    spans.push(Span::styled(
                        format!(
                            " · {comments} comment{}",
                            if comments == 1 { "" } else { "s" }
                        ),
                        palette.muted,
                    ));
                }
                draw_spans(layout.chips, surface, &spans);
            }
        }
        if !self.editing {
            let rows = self.draft_rows(layout.input.width as usize);
            for (index, row) in rows.iter().take(layout.input.height as usize).enumerate() {
                surface.set_spans(
                    layout.input.x,
                    layout.input.y + index as u16,
                    &row.spans,
                    layout.input.width,
                );
            }
        }
        if layout.meta.height > 0 {
            draw_spans(layout.meta, surface, &self.metadata(layout.meta.width));
        }
    }

    /// `Build · model provider · variant`, falling back to the last step's selection.
    fn metadata(&self, width: u16) -> Vec<Span<'static>> {
        let palette = self.palette;
        let session = self.state.session.as_ref();
        let last = self
            .state
            .messages
            .iter()
            .rev()
            .find(|message| message.kind == "assistant");
        let agent = session
            .and_then(|session| session.agent.as_deref())
            .or_else(|| last.and_then(|message| message.agent.as_deref()));
        let model = session
            .and_then(|session| session.model.as_ref())
            .or_else(|| last.and_then(|message| message.model.as_ref()));
        let mut spans = Vec::new();
        if let Some(agent) = agent.filter(|_| width >= 44) {
            spans.push(Span::styled(transcript::titlecase(agent), palette.agent));
            spans.push(Span::styled(" · ", palette.muted));
        }
        match model {
            Some(model) => {
                spans.push(Span::styled(model.model.clone(), palette.text));
                if width >= 44 {
                    spans.push(Span::styled(format!(" {}", model.provider), palette.muted));
                }
                if let Some(variant) = model
                    .variant
                    .as_deref()
                    .filter(|variant| *variant != "default")
                {
                    spans.push(Span::styled(" · ", palette.muted));
                    spans.push(Span::styled(
                        variant.to_owned(),
                        palette.warning.add_modifier(Modifier::BOLD),
                    ));
                }
            }
            None => spans.push(Span::styled("Default model", palette.muted)),
        }
        spans
    }

    fn usage(&self) -> Option<String> {
        let tokens = self
            .state
            .messages
            .iter()
            .rev()
            .filter(|message| message.kind == "assistant")
            .find_map(|message| message.tokens.filter(|tokens| tokens.total() > 0.0))
            .map(|tokens| tokens.total());
        let cost: f64 = self
            .state
            .messages
            .iter()
            .filter_map(|message| message.cost)
            .sum();
        let mut parts = Vec::new();
        if let Some(tokens) = tokens {
            parts.push(transcript::number(tokens));
        }
        if cost > 0.0 {
            parts.push(format!("${cost:.2}"));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// Running state or location on the left; usage and key hints on the right.
    fn footer(&self, area: Rect, surface: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let palette = self.palette;
        let key = |key: &'static str, label: &'static str| {
            [
                Span::styled(key, palette.text),
                Span::styled(label, palette.muted),
            ]
        };
        let hints: Vec<Span> = if self.editing {
            [key("enter", " send  "), key("esc", " cancel")].concat()
        } else if self.selecting {
            [
                key("enter", " open  "),
                key("]/[", " select  "),
                key("i", " write"),
            ]
            .concat()
        } else if self.expanded {
            [key("i", " write  "), key("t", " less  "), key("q", " hide")].concat()
        } else {
            [
                key("i", " write  "),
                key("t", " details  "),
                key("q", " hide"),
            ]
            .concat()
        };
        let mut right = Vec::new();
        if let Some(usage) = self.usage() {
            right.push(Span::styled(format!("{usage}  "), palette.muted));
        }
        right.extend(hints.iter().cloned());
        let left = self.status();
        let left_width = spans_width(&left).min(area.width as usize / 2).max(12);
        let budget = (area.width as usize).saturating_sub(left_width + 2);
        // Drop usage, then all hints, when the footer is narrow.
        let right = if spans_width(&right) <= budget {
            right
        } else if spans_width(&hints) <= budget {
            hints
        } else {
            Vec::new()
        };
        let right_width = spans_width(&right) as u16;
        draw_spans(
            Rect::new(area.right() - right_width, area.y, right_width, 1),
            surface,
            &right,
        );
        let left_area = area.with_width(area.width.saturating_sub(right_width + 2));
        draw_spans(left_area, surface, &left);
    }

    fn status(&self) -> Vec<Span<'static>> {
        let palette = self.palette;
        if !self.state.connected {
            if self.state.status.starts_with("Connecting") {
                return vec![Span::styled(
                    format!("{} Connecting…", transcript::spinner_frame()),
                    palette.muted,
                )];
            }
            return vec![
                Span::styled("○ Offline  ", palette.error),
                Span::styled("r", palette.text),
                Span::styled(" reconnect", palette.muted),
            ];
        }
        if self.state.busy {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(scanner(palette));
            spans.push(Span::styled(" c", palette.text));
            spans.push(Span::styled(" interrupt", palette.muted));
            return spans;
        }
        vec![Span::styled(
            home_relative(self.session_directory()),
            palette.muted,
        )]
    }
}

/// The TUI's eight-cell "knight rider" activity indicator.
fn scanner(palette: &Palette) -> Vec<Span<'static>> {
    const CELLS: u64 = 8;
    let tick = opencode::now_millis() / opencode::SPINNER_INTERVAL_MS;
    let phase = tick % (CELLS * 2 - 2);
    let forward = phase < CELLS;
    let head = if forward {
        phase
    } else {
        CELLS * 2 - 2 - phase
    };
    (0..CELLS)
        .map(|cell| {
            let trail = if forward {
                cell + 1 == head
            } else {
                cell == head + 1
            };
            if cell == head {
                Span::styled("■", palette.agent)
            } else if trail {
                Span::styled("■", palette.agent.add_modifier(Modifier::DIM))
            } else {
                Span::styled("⬝", palette.muted)
            }
        })
        .collect()
}

fn home_relative(path: &std::path::Path) -> String {
    helix_stdx::path::fold_home_dir(path)
        .to_string_lossy()
        .trim_end_matches('/')
        .to_owned()
}

/// Plain wrapped rows for pane-local views, inset from the panel edge.
fn append(lines: &mut Vec<ChatRow>, text: &str, width: usize, style: Style) {
    for line in text.replace('\t', "    ").split('\n') {
        let start = lines.len();
        transcript::wrap(
            lines,
            &Spans::from(Span::styled(line, style)),
            &[],
            width,
            0,
        );
        for row in &mut lines[start..] {
            row.indent = 2;
        }
    }
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

fn draw_spans(area: Rect, surface: &mut Buffer, spans: &[Span<'_>]) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut x = area.x;
    for span in spans {
        let available = area.right().saturating_sub(x);
        if available == 0 {
            break;
        }
        let width = span.content.width() as u16;
        if width > available {
            write(
                Rect::new(x, area.y, available, 1),
                surface,
                &span.content,
                span.style,
            );
            break;
        }
        write(
            Rect::new(x, area.y, width, 1),
            surface,
            &span.content,
            span.style,
        );
        x += width;
    }
}

fn fill(area: Rect, surface: &mut Buffer, style: Style) {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = surface.get_mut(x, y) {
                cell.reset();
                cell.set_style(style);
            }
        }
    }
}

fn write(area: Rect, surface: &mut Buffer, text: &str, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Chrome is single-line: ellipsize instead of silently showing only the
    // first wrapped row. Strip controls and truncate only at grapheme boundaries.
    let clean: String = text
        .replace('\t', "    ")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let truncated = clean.width() > area.width as usize;
    let available = area.width as usize - usize::from(truncated);
    let mut label = String::new();
    let mut used = 0;
    for grapheme in clean.graphemes(true) {
        if used + grapheme.width() > available {
            break;
        }
        label.push_str(grapheme);
        used += grapheme.width();
    }
    if truncated {
        label.push('…');
    }
    surface.set_stringn(area.x, area.y, &label, area.width as usize, style);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::{Message, Part};

    fn part(value: serde_json::Value) -> Part {
        serde_json::from_value(value).unwrap()
    }

    fn loader() -> Arc<ArcSwap<syntax::Loader>> {
        Arc::new(ArcSwap::from_pointee(syntax::Loader::default()))
    }

    fn palette() -> Palette {
        Palette::new(&helix_view::theme::DEFAULT_THEME)
    }

    fn view<'a>(
        state: &'a State,
        loader: &'a Arc<ArcSwap<syntax::Loader>>,
        palette: &'a Palette,
    ) -> ChatView<'a> {
        ChatView {
            state,
            loader,
            palette,
            draft: "",
            directory: std::path::Path::new("/work/helix"),
            theme: &helix_view::theme::DEFAULT_THEME,
            expanded: false,
            focused: true,
            editing: false,
            snippets: &[],
            selecting: false,
        }
    }

    fn row_text(row: &ChatRow) -> String {
        row.spans
            .0
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn text(rows: &[ChatRow]) -> String {
        rows.iter().map(row_text).collect::<Vec<_>>().join("\n")
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

    fn assistant(content: Vec<serde_json::Value>) -> Message {
        Message {
            id: "msg_assistant".into(),
            kind: "assistant".into(),
            content: content.into_iter().map(part).collect(),
            ..Message::default()
        }
    }

    #[test]
    fn user_messages_are_shaded_blocks_with_an_agent_rail() {
        let mut state = State::default();
        state.messages.push(Message {
            id: "msg_user".into(),
            kind: "user".into(),
            text: "hello 世界\nnext line".into(),
            ..Message::default()
        });
        state.error = Some("connection failed".into());
        let (loader, palette) = (loader(), palette());
        let rows = view(&state, &loader, &palette).transcript(12);
        assert!(
            !text(&rows).contains("You"),
            "The TUI does not label user messages"
        );
        let first = rows
            .iter()
            .position(|row| row_text(row).contains("hello"))
            .unwrap();
        assert_eq!(rows[first].indent, transcript::INDENT);
        assert_eq!(rows[first].rail.map(|(column, _)| column), Some(0));
        assert!(rows[first].fill.is_some());
        assert!(rows[first - 1].fill.is_some(), "Padding row above the text");
        assert!(rows
            .iter()
            .all(|row| row.spans.width() + row.indent as usize <= 12));
        let wide = view(&state, &loader, &palette).transcript(40);
        assert!(text(&wide).contains("Error: connection failed"));
        assert!(wide
            .iter()
            .flat_map(|row| &row.spans.0)
            .any(|span| span.content.contains("connection") && span.style == palette.error));
        let mut narrow = Vec::new();
        transcript::wrap(&mut narrow, &Spans::from(Span::raw("世a\u{1b}")), &[], 1, 0);
        assert!(narrow
            .iter()
            .all(|row| row.spans.width() <= 1 && !row_text(row).contains('\u{1b}')));
    }

    #[test]
    fn markdown_colors_and_nested_emphasis_follow_the_editor_theme() {
        let mut state = State::default();
        state.messages.push(assistant(vec![serde_json::json!({"type":"text","text":
            "## Colorful **heading**\n\n**Important _detail_** and `code` with [website](https://example.com).\n\n- First item"
        })]));
        let (loader, palette) = (loader(), palette());
        let chat = view(&state, &loader, &palette);
        let rows = chat.transcript(120);
        let spans: Vec<_> = rows.iter().flat_map(|row| &row.spans.0).collect();
        let styled = |text: &str| {
            spans
                .iter()
                .find(|span| span.content.contains(text))
                .unwrap()
                .style
        };
        let theme = chat.theme;
        assert_eq!(styled("Colorful").fg, theme.get("markup.heading").fg);
        assert_eq!(styled("Important").fg, theme.get("constant").fg);
        assert_eq!(styled("detail").fg, theme.get("constant").fg);
        assert!(styled("detail")
            .add_modifier
            .contains(Modifier::BOLD | Modifier::ITALIC));
        assert_eq!(styled("code").fg, theme.get("markup.raw").fg);
        assert_eq!(styled("website").fg, theme.get("function").fg);
        assert_eq!(
            styled("website").underline_style,
            Some(UnderlineStyle::Line)
        );
        assert_eq!(styled("•").fg, theme.get("function").fg);
        assert!(rows
            .iter()
            .filter(|row| !row.spans.0.is_empty())
            .all(|row| row.indent == transcript::INDENT));
    }

    #[test]
    fn list_continuations_keep_their_indent_and_link_targets() {
        let mut state = State::default();
        state.messages.push(assistant(vec![serde_json::json!({"type":"text","text":
            "- Read [the implementation and its tests](src/main.rs:3) before changing it.\n\nA soft\nbreak stays in its paragraph."
        })]));
        let (loader, palette) = (loader(), palette());
        let rows = view(&state, &loader, &palette).transcript(26);
        let list_start = rows
            .iter()
            .position(|row| row_text(row).starts_with("• "))
            .unwrap();
        let continuation = row_text(&rows[list_start + 1]);
        assert!(continuation.starts_with("  "), "{continuation:?}");
        assert!(rows
            .iter()
            .flat_map(|row| &row.links)
            .any(|(_, reference)| reference.target.label() == "src/main.rs:3"));
        assert!(rows
            .iter()
            .all(|row| row.spans.width() + row.indent as usize <= 26));
        let wide = view(&state, &loader, &palette).transcript(100);
        assert!(wide
            .iter()
            .any(|row| row_text(row).contains("A soft break stays")));
    }

    #[test]
    fn chat_theme_overrides_do_not_restyle_other_markdown() {
        let theme = Theme::from(
            toml::from_str::<toml::Value>(
                r##"
            "ui.text" = "#eeeeee"
            "keyword" = "#aa00aa"
            "markup.heading" = "#bb00bb"
            "ui.opencode.heading" = "#00bb00"
            "ui.opencode.accent" = "#00aaaa"
        "##,
            )
            .unwrap(),
        );
        let markdown = super::super::markdown::Markdown::new("## Title".into(), loader());
        assert_eq!(
            markdown.parse(Some(&theme)).lines[0].0[0].style.fg,
            theme.get("markup.heading").fg
        );
        let chat = markdown.for_chat();
        assert_eq!(
            chat.parse(Some(&theme)).lines[0].0[0].style.fg,
            theme.get("ui.opencode.heading").fg
        );
        assert_eq!(
            chat_style(&theme, "accent", "keyword").fg,
            theme.get("ui.opencode.accent").fg
        );
    }

    #[test]
    fn layout_stacks_transcript_jump_row_prompt_and_footer() {
        let layout = ChatLayout::new(Rect::new(0, 0, 47, 30), 1, 0, 0);
        assert_eq!(layout.body.width, 45);
        assert_eq!(layout.body.bottom(), layout.jump.y);
        assert_eq!(layout.jump.bottom(), layout.composer.y);
        assert_eq!(layout.composer.bottom(), layout.footer.y);
        assert_eq!(
            layout.composer.height, 5,
            "padding, input, padding, metadata, edge"
        );
        assert_eq!(layout.input.y, layout.composer.y + 1);
        assert_eq!(layout.meta.y, layout.composer.y + 3);
        assert_eq!(layout.edge.y, layout.composer.bottom() - 1);
        let docked = ChatLayout::new(Rect::new(0, 0, 47, 30), 2, 1, 6);
        assert_eq!(docked.dock.height, 6);
        assert_eq!(docked.dock.bottom() + 1, docked.composer.y);
        assert_eq!(docked.input.height, 2);
        assert_eq!(docked.chips.y, docked.composer.y + 1);
    }

    #[test]
    fn activity_collapses_into_a_summary_and_expands_into_tool_rows() {
        let mut state = State::default();
        let mut message = assistant(vec![
            serde_json::json!({"type":"reasoning", "text":"**Inspect**\n\nCheck the failing connection"}),
            serde_json::json!({"type":"tool", "name":"read", "state":{
                "status":"completed", "input":{"path":"src/main.rs","offset":10,"limit":5},
                "content":[{"type":"text","text":"output line\n".repeat(1000)}]
            }}),
            serde_json::json!({"type":"tool", "name":"opencode.read", "state":{"status":"completed","input":{"path":"two.rs"}}}),
            serde_json::json!({"type":"tool", "name":"bash", "state":{
                "status":"completed", "input":{"command":"cargo test"},
                "content":[{"type":"text","text":(1..=30).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n")}]
            }}),
            serde_json::json!({"type":"tool", "name":"grep", "state":{
                "status":"completed", "input":{"pattern":"fn main"}, "metadata":{"matches":3}
            }}),
            serde_json::json!({"type":"text","text":"Done."}),
        ]);
        message.time.completed = Some(1);
        state.messages.push(message);
        let (loader, palette) = (loader(), palette());
        let mut view = view(&state, &loader, &palette);
        let compact = text(&view.transcript(60));
        assert!(
            compact.contains("+ 1 command, 1 thought, 2 reads, 1 tool"),
            "{compact}"
        );
        assert!(!compact.contains("src/main.rs"));
        assert!(!compact.contains("output line"));
        assert!(compact.contains("Done."));
        view.expanded = true;
        let rows = view.transcript(60);
        let expanded = text(&rows);
        assert!(expanded.contains("− 1 command"));
        assert!(expanded.contains("Thought: Inspect"));
        assert!(expanded.contains("Check the failing connection"));
        assert!(expanded.contains("→ Read src/main.rs:10-14"));
        assert!(expanded.contains("→ Read two.rs"));
        assert!(expanded.contains("$ cargo test"));
        assert!(expanded.contains("earlier lines"));
        assert!(expanded.contains("line 30"));
        assert!(!expanded.contains("line 1\n"));
        assert!(expanded.contains("✱ Grep \"fn main\" (3 matches)"));
        assert!(
            !expanded.contains("output line"),
            "Read output stays hidden"
        );
        assert!(rows.len() < 60);
        assert!(rows
            .iter()
            .flat_map(|row| &row.links)
            .any(|(_, reference)| reference.target.label() == "src/main.rs"));
    }

    #[test]
    fn running_work_shows_its_status_and_completed_steps_show_a_footer() {
        let mut state = State::default();
        state.busy = true;
        state.messages.push(Message {
            id: "msg_prompt".into(),
            kind: "user".into(),
            text: "Fix it".into(),
            time: opencode::Times {
                created: Some(1_000),
                ..Default::default()
            },
            ..Message::default()
        });
        let mut step = assistant(vec![serde_json::json!({"type":"text","text":"Fixed."})]);
        step.id = "msg_step".into();
        step.agent = Some("build".into());
        step.model = Some(opencode::ModelRef {
            provider: "anthropic".into(),
            model: "claude".into(),
            variant: None,
        });
        step.finish = Some("stop".into());
        step.time.completed = Some(13_400);
        state.messages.push(step);
        let mut running = assistant(vec![
            serde_json::json!({"type":"tool","name":"bash","state":{
                "status":"running","input":{"command":"cargo build"}
            }}),
        ]);
        running.id = "msg_running".into();
        running.finish = Some("tool-calls".into());
        state.messages.push(running);
        let (loader, palette) = (loader(), palette());
        let rows = view(&state, &loader, &palette).transcript(80);
        let output = text(&rows);
        assert!(
            output.contains("Build · anthropic/claude · 12.4s"),
            "{output}"
        );
        assert!(output.contains("Running command…"));
        assert_eq!(
            output.matches("Build ·").count(),
            1,
            "Tool-call steps have no footer"
        );
    }

    #[test]
    fn edit_tools_render_a_numbered_diff_block() {
        let mut state = State::default();
        state.messages.push(assistant(vec![serde_json::json!({"type":"tool","name":"edit","state":{
            "status":"completed","input":{"path":"/work/helix/src/lib.rs"},
            "content":[{"type":"text","text":"ok"}],
            "metadata":{"files":[{"type":"update","relativePath":"src/lib.rs","filePath":"/work/helix/src/lib.rs",
                "patch":"--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -8,3 +8,3 @@\n context\n-old line\n+new line\n","additions":1,"deletions":1}]}
        }})]));
        let (loader, palette) = (loader(), palette());
        let mut view = view(&state, &loader, &palette);
        view.expanded = true;
        let rows = view.transcript(60);
        let output = text(&rows);
        assert!(output.contains("← Edit src/lib.rs"), "{output}");
        assert!(output.contains(" 8   context"));
        assert!(output.contains(" 9 - old line"));
        assert!(output.contains(" 9 + new line"));
        assert!(!output.contains("+++"));
        let added = rows
            .iter()
            .find(|row| row_text(row).contains("new line"))
            .unwrap();
        assert_eq!(
            added.fill.map(|(column, _)| column),
            Some(transcript::INDENT)
        );
        assert!(added
            .spans
            .0
            .iter()
            .any(|span| span.style.fg == palette.diff_plus.fg));
    }

    #[test]
    fn selection_comments_render_separately_from_the_display_message() {
        let mut state = State::default();
        state.messages.push(serde_json::from_value(serde_json::json!({
            "id":"msg_comment","type":"user",
            "text":"Overall message\nThe user made the following comment regarding line 4 of /work/source.rs: Why this branch?",
            "metadata":{"displayText":"Overall message","comments":[{
                "path":"/work/source.rs","selection":{"startLine":4,"endLine":4,"startChar":0,"endChar":10},
                "comment":"Why this branch?","preview":"if branch {","origin":"file"
            }],"attachments":[]}
        })).unwrap());
        let (loader, palette) = (loader(), palette());
        let output = text(&view(&state, &loader, &palette).transcript(160));
        assert!(output.contains("Overall message"));
        assert!(output.contains("↳ /work/source.rs:4: Why this branch?"));
        assert!(
            !output.contains("The user made the following comment"),
            "Model-facing scaffolding must not be shown as the user's overall message"
        );
        assert_eq!(output.matches("Why this branch?").count(), 1);
    }

    #[test]
    fn markdown_file_links_are_inline_independent_and_survive_unicode_wrapping() {
        let mut state = State::default();
        let mut message = assistant(vec![serde_json::json!({"type":"text","text":
            "See [**é界 implementation**](src/first.rs#L12C3) and [other](src/second.rs:8).\n\n`src/third.rs:4` and src/fourth.rs:9."
        })]);
        message.id = "msg_links".into();
        state.messages.push(message);
        let (loader, palette) = (loader(), palette());
        for width in [12, 24, 200] {
            let rows = view(&state, &loader, &palette).transcript(width);
            let rendered = text(&rows);
            assert!(!rendered.contains("](src/"));
            let links: Vec<_> = rows
                .iter()
                .flat_map(|row| {
                    row.links.iter().map(move |(span, reference)| {
                        assert!(*span < row.spans.0.len());
                        assert!(row.spans.0[*span].content.width() <= width);
                        (
                            row.spans.0[*span].content.to_string(),
                            reference.target.label(),
                        )
                    })
                })
                .collect();
            for target in [
                "src/first.rs:12:3",
                "src/second.rs:8",
                "src/third.rs:4",
                "src/fourth.rs:9",
            ] {
                assert!(
                    links.iter().any(|(_, label)| label == target),
                    "missing {target} at width {width}"
                );
            }
            assert!(links
                .iter()
                .any(|(text, target)| target == "src/first.rs:12:3" && text.contains('é')));
        }
    }

    #[test]
    fn web_image_labels_and_code_blocks_are_not_local_links() {
        let mut state = State::default();
        state.messages.push(assistant(vec![serde_json::json!({"type":"text","text":
            "[src/web.rs](https://example.com) ![src/image.rs](image.png)\n\n```rust\nsrc/not-a-link.rs\n```\n\n[local](src/local.rs)"
        })]));
        let (loader, palette) = (loader(), palette());
        let rows = view(&state, &loader, &palette).transcript(100);
        let links: Vec<_> = rows.iter().flat_map(|row| &row.links).collect();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1.target.path.to_str(), Some("src/local.rs"));
    }

    #[test]
    fn chrome_ellipsizes_unicode_without_overwriting_neighbors() {
        let area = Rect::new(2, 1, 6, 1);
        let mut surface = Buffer::empty(Rect::new(0, 0, 10, 3));
        surface.set_string(8, 1, "x", Style::default());
        write(area, &mut surface, "é界 long title\u{1b}", Style::default());
        assert_eq!(surface.get(7, 1).unwrap().symbol.as_str(), "…");
        assert_eq!(surface.get(8, 1).unwrap().symbol.as_str(), "x");
    }

    #[test]
    fn empty_state_and_composer_are_persistent_and_show_drafts() {
        let mut state = State::default();
        state.connected = true;
        let (loader, palette) = (loader(), palette());
        let mut view = view(&state, &loader, &palette);
        let area = Rect::new(0, 0, 64, 24);
        let layout = view.layout(area);
        let mut surface = Buffer::empty(area);
        view.render(area, &mut surface, &layout, &[], 0, true);
        let output = screen(&surface);
        assert!(output.contains("What would you like"));
        assert!(output.contains("Ask anything…"));
        assert!(output.contains("Default model"));
        assert!(
            output.contains("▀▀▀"),
            "The prompt has a half-height lower edge"
        );
        assert!(output.contains("i write"));
        assert!(!output.contains("Jump to latest"));
        view.draft = "Review the current changes";
        view.render(area, &mut surface, &layout, &[], 0, false);
        let output = screen(&surface);
        assert!(output.contains("Review the current changes"));
        assert!(!output.contains("Ask anything"));
        assert!(output.contains("Jump to latest ↓"));
        view.editing = true;
        view.render(area, &mut surface, &layout, &[], 0, true);
        let output = screen(&surface);
        assert!(output.contains("enter send"));
        assert!(
            !output.contains("Review the current changes"),
            "The preview must not duplicate the live input overlay"
        );
        let input = layout.input_area();
        assert!(input.y > layout.composer.y);
        assert!(input.bottom() < layout.composer.bottom());
        assert!(
            input.bottom() < area.bottom() - 1,
            "Input belongs in the panel, not on the global command line"
        );
    }

    #[test]
    fn pending_requests_dock_above_the_prompt() {
        let mut state = State::default();
        state.connected = true;
        state.forms.insert(
            "frm_1".into(),
            serde_json::from_value(serde_json::json!({
                "id":"frm_1","sessionID":"ses_test","title":"Pick a branch","fields":[]
            }))
            .unwrap(),
        );
        let (loader, palette) = (loader(), palette());
        let view = view(&state, &loader, &palette);
        let area = Rect::new(0, 0, 64, 24);
        let layout = view.layout(area);
        assert!(layout.dock.height > 0);
        let mut surface = Buffer::empty(area);
        view.render(area, &mut surface, &layout, &[], 0, true);
        let output = screen(&surface);
        assert!(output.contains("△ Question"));
        assert!(output.contains("? Pick a branch"));
        assert!(output.contains("p respond"));
    }

    #[test]
    fn small_and_offset_panes_never_draw_outside_their_area() {
        let mut state = State::default();
        state.error = Some("Network unavailable".into());
        state.busy = true;
        state.connected = true;
        state.messages.push(Message {
            id: "msg_user".into(),
            kind: "user".into(),
            text: "A long user request that wraps".into(),
            ..Message::default()
        });
        let (loader, palette) = (loader(), palette());
        let mut view = view(&state, &loader, &palette);
        view.draft = "draft\nwith lines";
        for width in [0, 1, 2, 5, 8, 20, 64] {
            for height in [0, 1, 2, 4, 8, 12, 24] {
                let area = Rect::new(2, 2, width, height);
                let outer = Rect::new(0, 0, width + 4, height + 4);
                let mut surface = Buffer::empty(outer);
                for y in 0..outer.height {
                    surface.set_string(0, y, &"x".repeat(outer.width as usize), Style::default());
                }
                let layout = view.layout(area);
                for rect in [
                    layout.body,
                    layout.jump,
                    layout.dock,
                    layout.composer,
                    layout.input,
                    layout.meta,
                    layout.edge,
                    layout.footer,
                ] {
                    assert!(
                        rect.height == 0 || (rect.y >= area.y && rect.bottom() <= area.bottom()),
                        "{rect:?} outside {area:?}"
                    );
                }
                let rows = view.transcript(layout.body.width as usize);
                view.render(area, &mut surface, &layout, &rows, 0, false);
                for y in 0..outer.height {
                    for x in 0..outer.width {
                        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
                            assert_eq!(
                                surface.get(x, y).unwrap().symbol.as_str(),
                                "x",
                                "outside {area:?} at {x},{y}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn composing_is_pane_local_and_scrolling_can_leave_follow_mode() -> anyhow::Result<()> {
        let mut connection = Connection::for_test();
        assert!(connection.send("hello".into()).is_err());
        connection.state.lock().connected = true;
        let mut pane = OpenCode {
            session_id: connection.id.clone(),
            directory: "/test".into(),
            connection,
            draft: Arc::new(Mutex::new(String::new())),
            input: Arc::new(Mutex::new(super::super::prompt::PromptAnchor::default())),
            panel: Arc::new(Mutex::new(Rect::default())),
            keys: KeyTrie::Node(Default::default()),
            offset: 30,
            follow: true,
            viewport_height: 10,
            expanded: false,
            references: Vec::new(),
            messages: Vec::new(),
            selected_reference: None,
            selected_message: None,
            snippets: Vec::new(),
        };
        pane.command(PaneCommand::Up, 3)?;
        assert_eq!(pane.offset, 27);
        assert!(!pane.follow);
        pane.references = vec![
            (
                references::Reference {
                    message_id: "msg_first".into(),
                    directory: "/work".into(),
                    target: references::FileTarget {
                        path: "first.rs".into(),
                        line: Some(2),
                        column: None,
                    },
                },
                30,
            ),
            (
                references::Reference {
                    message_id: "msg_second".into(),
                    directory: "/work".into(),
                    target: references::FileTarget {
                        path: "second.rs".into(),
                        line: None,
                        column: None,
                    },
                },
                50,
            ),
        ];
        pane.messages = vec![("msg_first".into(), 10), ("msg_second".into(), 40)];
        pane.command(PaneCommand::NextReference, 1)?;
        assert_eq!(
            pane.selected_reference.as_ref().unwrap().message_id,
            "msg_first"
        );
        assert_eq!(pane.offset, 29);
        pane.command(PaneCommand::NextReference, 100)?;
        assert_eq!(
            pane.selected_reference.as_ref().unwrap().message_id,
            "msg_second"
        );
        pane.command(PaneCommand::PreviousReference, 1)?;
        assert_eq!(
            pane.selected_reference.as_ref().unwrap().message_id,
            "msg_first"
        );
        pane.command(PaneCommand::NextMessage, 1)?;
        assert!(pane.selected_reference.is_none());
        assert_eq!(pane.selected_message.as_deref(), Some("msg_second"));
        assert_eq!(pane.offset, 40);
        pane.command(PaneCommand::PreviousMessage, 1)?;
        assert_eq!(pane.offset, 10);
        pane.command(PaneCommand::Right, 1)?;
        assert!(pane.expanded);
        pane.command(PaneCommand::Left, 1)?;
        assert!(!pane.expanded);
        let snippet = opencode::snippets::Snippet {
            comment: String::new(),
            path: Some("/test/source.rs".into()),
            text: "selected text".into(),
            selection: opencode::snippets::Selection {
                start_line: 2,
                end_line: 2,
                start_char: 0,
                end_char: 13,
            },
        };
        pane.stage_snippets(vec![snippet.clone(), snippet.clone()], "Review this")?;
        assert_eq!(
            pane.snippets.len(),
            1,
            "Repeated staging deduplicates identical snapshots"
        );
        assert_eq!(pane.draft.lock().as_str(), "Review this");
        assert!(!pane.needs_session_confirmation(&pane.session_id));
        assert!(pane.needs_session_confirmation("ses_other"));
        if let PaneOutcome::Compose { draft, .. } = pane.command(PaneCommand::Compose, 1)? {
            *draft.lock() = "cancelled draft".into();
        } else {
            panic!("compose must return a prompt");
        }
        assert_eq!(pane.draft.lock().as_str(), "cancelled draft");
        assert_eq!(
            pane.snippets.as_slice(),
            std::slice::from_ref(&snippet),
            "Cancelling composition keeps context"
        );
        pane.connection.state.lock().connected = false;
        assert!(pane.submit("hello".into()).is_err());
        assert_eq!(
            pane.snippets.as_slice(),
            std::slice::from_ref(&snippet),
            "Disconnected submission keeps context"
        );
        pane.connection.state.lock().connected = true;
        pane.update_snippet_comment(0, &snippet, "Explain this expression".into())?;
        assert_eq!(
            pane.draft.lock().as_str(),
            "cancelled draft",
            "Reference comments must not replace the main draft"
        );
        assert!(pane
            .update_snippet_comment(0, &snippet, "stale update".into())
            .is_err());
        pane.stage_snippets(vec![snippet.clone()], "")?;
        assert_eq!(
            pane.snippets[0].comment, "Explain this expression",
            "Reattaching without a comment preserves the existing one"
        );
        let mut replacement = snippet;
        replacement.comment = "Different comment".into();
        pane.stage_snippets(vec![replacement], "")?;
        assert_eq!(pane.snippets.len(), 1);
        assert_eq!(pane.snippets[0].comment, "Different comment");
        let session = pane.session_id.clone();
        let connection = pane.connection.state.clone();
        pane.offset = 17;
        pane.follow = false;
        pane.expanded = true;
        pane.input.lock().area = Rect::new(2, 20, 40, 1);
        assert!(pane.retain_on_close());
        pane.on_hide();
        assert_eq!(pane.session_id, session);
        assert!(Arc::ptr_eq(&connection, &pane.connection.state));
        assert_eq!(pane.draft.lock().as_str(), "cancelled draft");
        assert_eq!(pane.snippets[0].comment, "Different comment");
        assert_eq!(pane.offset, 17);
        assert!(!pane.follow);
        assert!(pane.expanded);
        assert_eq!(pane.input.lock().area, Rect::default());
        pane.submit("hello".into())?;
        assert_eq!(pane.connection.state.lock().pending[0].text, "hello");
        assert_eq!(pane.connection.state.lock().pending[0].snippets.len(), 1);
        assert_eq!(
            pane.connection.state.lock().pending[0].snippets[0].comment,
            "Different comment"
        );
        assert!(
            pane.snippets.is_empty(),
            "Sent context is not attached to subsequent prompts"
        );
        assert!(pane.follow);
        Ok(())
    }
}
