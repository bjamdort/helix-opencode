use std::{path::PathBuf, sync::Arc};
mod interactions;
pub(crate) mod models;
pub(super) mod references;
pub(crate) mod snippets;

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

use super::pane::{Pane, PaneOutcome};
use crate::{
    commands::pane::PaneCommand,
    job::Jobs,
    keymap::{macros::keymap, KeyTrie},
    opencode::{self, Connection, State},
};

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
    fn max_width(&self, available: u16) -> u16 {
        (available / 2).min(available.saturating_sub(20))
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
        let layout = ChatLayout::new(area);
        let editing = {
            let mut input = self.input.lock();
            input.area = layout.input_area();
            input.active
        };
        let state = self.connection.state.lock();
        let draft = self.draft.lock();
        let view = ChatView {
            state: &state,
            draft: &draft,
            directory: &self.directory,
            theme: &editor.theme,
            loader: &editor.syn_loader,
            expanded: self.expanded,
            focused,
            editing,
            snippets: &self.snippets,
        };
        self.viewport_height = layout.body.height.max(1) as usize;
        let mut lines = view.transcript(layout.body.width.saturating_sub(3) as usize);
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
                    row.rail = Some(editor.theme.get("ui.text.focus"));
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
        if self.selected_reference.is_some() && !editing {
            fill(layout.footer, surface, editor.theme.get("ui.background"));
            write(
                layout.footer,
                surface,
                "Enter/o open file   ]/[ select   i message",
                editor.theme.get("ui.text.info"),
            );
        }
    }

    fn cursor(&self, area: Rect) -> (Option<Position>, CursorKind) {
        (
            Some(Position::new(area.y as usize, area.x as usize)),
            CursorKind::Hidden,
        )
    }
}

/// Reserve the composer even when the transcript scrolls. Small terminals use
/// compact chrome instead of allowing rectangles to overlap or underflow.
struct ChatLayout {
    header: Rect,
    body: Rect,
    composer: Rect,
    footer: Rect,
}

impl ChatLayout {
    fn new(area: Rect) -> Self {
        let inner = area.clip_left(2).clip_right(1);
        let header_height = if inner.height >= 12 { 3 } else { 1 }.min(inner.height);
        let rest = inner.clip_top(header_height);
        let footer_height = u16::from(rest.height > 0);
        let composer_height =
            if rest.height >= 8 { 5 } else { 3 }.min(rest.height.saturating_sub(footer_height));
        Self {
            header: inner.with_height(header_height),
            body: rest.clip_bottom(composer_height + footer_height),
            composer: Rect::new(
                rest.x,
                rest.bottom()
                    .saturating_sub(composer_height + footer_height),
                rest.width,
                composer_height,
            ),
            footer: Rect::new(
                rest.x,
                rest.bottom().saturating_sub(footer_height),
                rest.width,
                footer_height,
            ),
        }
    }

    fn input_area(&self) -> Rect {
        let content = self.composer.clip_left(2).clip_right(1);
        if content.width < 3 || content.height < 3 {
            Rect::default()
        } else {
            content.clip_top(1).with_height(1)
        }
    }
}

#[derive(Default)]
struct ChatRow {
    spans: Spans<'static>,
    background: Style,
    rail: Option<Style>,
    links: Vec<(usize, references::Reference)>,
    message: Option<String>,
}

impl ChatRow {
    fn render(&self, area: Rect, surface: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        surface.set_style(area, self.background);
        if let Some(style) = self.rail {
            surface.set_stringn(area.x, area.y, "┃", area.width as usize, style);
        }
        let text = area.clip_left(3);
        surface.set_spans(text.x, text.y, &self.spans, text.width);
    }
}

struct ChatView<'a> {
    state: &'a State,
    draft: &'a str,
    directory: &'a std::path::Path,
    theme: &'a Theme,
    loader: &'a Arc<ArcSwap<syntax::Loader>>,
    expanded: bool,
    focused: bool,
    editing: bool,
    snippets: &'a [opencode::snippets::Snippet],
}

impl ChatView<'_> {
    fn transcript(&self, width: usize) -> Vec<ChatRow> {
        let theme = self.theme;
        let muted = theme.get("ui.text.inactive");
        let accent = theme.get("ui.text.info");
        let card = theme.get("ui.popup");
        let mut lines = Vec::new();
        for message in &self.state.messages {
            let start = lines.len();
            let directory = self
                .state
                .session
                .as_ref()
                .map_or(self.directory, |session| {
                    session.location.directory.as_path()
                });
            match message.kind.as_str() {
                "user" => {
                    let display = message
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("displayText"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(&message.text);
                    self.user_block(&mut lines, "You", display, width);
                    for file in &message.files {
                        let name = file
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("Attached file");
                        let reference =
                            references::from_attachment(file).map(|target| references::Reference {
                                message_id: message.id.clone(),
                                directory: directory.to_owned(),
                                target,
                            });
                        let label = reference
                            .as_ref()
                            .map_or_else(|| name.to_owned(), |reference| reference.target.label());
                        let spans = Spans::from(Span::styled(format!("▧ {label}"), muted));
                        wrap_linked_spans(&mut lines, &spans, &[reference], width);
                    }
                    render_selection_comments(&mut lines, message.metadata.as_ref(), width, muted);
                }
                "assistant" => {
                    let agent = message.agent.as_deref().unwrap_or("Assistant");
                    let header = message.model.as_ref().map_or_else(
                        || format!("◆ {agent}"),
                        |model| format!("◆ {agent}  ·  {}", model.label()),
                    );
                    append(
                        &mut lines,
                        &header,
                        width,
                        accent.add_modifier(Modifier::BOLD),
                    );
                    lines.push(ChatRow::default());
                    let mut reads = 0;
                    for part in &message.content {
                        if !self.expanded
                            && part.kind == "tool"
                            && part.name.rsplit('.').next() == Some("read")
                            && part
                                .state
                                .as_ref()
                                .is_some_and(|tool| tool.status == "completed")
                        {
                            reads += 1;
                            continue;
                        }
                        collapsed_reads(&mut lines, &mut reads, width, muted);
                        match part.kind.as_str() {
                            "text" => {
                                let markdown = super::markdown::Markdown::new(
                                    part.text.clone(),
                                    self.loader.clone(),
                                );
                                let (text, inline) = markdown.parse_with_inline(Some(theme));
                                for (line_index, line) in text.lines.iter().enumerate() {
                                    let mut spans = Vec::new();
                                    let mut links = Vec::new();
                                    for (span_index, span) in line.0.iter().enumerate() {
                                        let targets = inline
                                            .iter()
                                            .find(|item| {
                                                item.line == line_index && item.span == span_index
                                            })
                                            .map(|item| {
                                                references::inline_targets(
                                                    &span.content,
                                                    item.destination.as_deref(),
                                                    item.code,
                                                )
                                            })
                                            .unwrap_or_default();
                                        let mut offset = 0;
                                        for (range, target) in targets {
                                            if range.start > offset {
                                                spans.push(Span::styled(
                                                    span.content[offset..range.start].to_owned(),
                                                    span.style,
                                                ));
                                                links.push(None);
                                            }
                                            spans.push(Span::styled(
                                                span.content[range.clone()].to_owned(),
                                                span.style.underline_style(UnderlineStyle::Line),
                                            ));
                                            links.push(Some(references::Reference {
                                                message_id: message.id.clone(),
                                                directory: directory.to_owned(),
                                                target,
                                            }));
                                            offset = range.end;
                                        }
                                        if offset < span.content.len() {
                                            spans.push(Span::styled(
                                                span.content[offset..].to_owned(),
                                                span.style,
                                            ));
                                            links.push(None);
                                        }
                                    }
                                    wrap_linked_spans(
                                        &mut lines,
                                        &Spans::from(spans),
                                        &links,
                                        width,
                                    );
                                }
                            }
                            "reasoning" => {
                                let done = message.time.completed.is_some()
                                    || message
                                        .content
                                        .iter()
                                        .any(|p| p.kind == "text" && !p.text.is_empty());
                                let label = if done { "Thought" } else { "Thinking…" };
                                append(
                                    &mut lines,
                                    &format!("{} {label}", if self.expanded { "▾" } else { "▸" }),
                                    width,
                                    muted,
                                );
                                if self.expanded {
                                    let start = lines.len();
                                    append(
                                        &mut lines,
                                        &part.text.replace("[REDACTED]", ""),
                                        width,
                                        muted.add_modifier(Modifier::ITALIC),
                                    );
                                    for row in &mut lines[start..] {
                                        row.rail = Some(theme.get("ui.window"));
                                    }
                                }
                            }
                            "tool" => {
                                let tool = part.state.as_ref();
                                let status = tool.map_or("pending", |tool| tool.status.as_str());
                                let (icon, style) = match status {
                                    "completed" => ("✓", theme.get("diagnostic.hint")),
                                    "error" => ("×", theme.get("error")),
                                    _ => ("⋯", accent),
                                };
                                let input = tool.and_then(|tool| tool.input.as_ref());
                                let summary = input.and_then(|input| {
                                    ["path", "file_path", "command", "pattern", "query"]
                                        .iter()
                                        .find_map(|key| {
                                            input.get(key).and_then(serde_json::Value::as_str)
                                        })
                                });
                                let label = summary.map_or_else(
                                    || part.name.clone(),
                                    |summary| {
                                        let summary: String = summary
                                            .lines()
                                            .next()
                                            .unwrap_or_default()
                                            .chars()
                                            .take(120)
                                            .collect();
                                        format!("{}  {}", part.name, summary)
                                    },
                                );
                                let summary_start = lines.len();
                                append(
                                    &mut lines,
                                    &format!("{icon} {label}  ·  {status}"),
                                    width,
                                    style,
                                );
                                if let Some(target) = input
                                    .and_then(|input| {
                                        ["path", "file_path", "filePath", "filename"]
                                            .iter()
                                            .find_map(|key| {
                                                input.get(key).and_then(serde_json::Value::as_str)
                                            })
                                    })
                                    .and_then(|path| references::parse(path, true))
                                {
                                    let reference = references::Reference {
                                        message_id: message.id.clone(),
                                        directory: directory.to_owned(),
                                        target,
                                    };
                                    // Keep a tool's file target on its existing summary, not a duplicate row.
                                    for row in &mut lines[summary_start..] {
                                        for (index, span) in row.spans.0.iter_mut().enumerate() {
                                            span.style =
                                                span.style.underline_style(UnderlineStyle::Line);
                                            row.links.push((index, reference.clone()));
                                        }
                                    }
                                }
                                if self.expanded {
                                    let start = lines.len();
                                    if let Some(input) = input {
                                        preview(
                                            &mut lines,
                                            &format!("Input: {input}"),
                                            width,
                                            muted,
                                        );
                                    }
                                    if let Some(tool) = tool {
                                        for content in tool.content.iter().take(4) {
                                            if let Some(output) = content
                                                .get("text")
                                                .and_then(serde_json::Value::as_str)
                                            {
                                                preview(&mut lines, output, width, muted);
                                            }
                                        }
                                        if let Some(error) = &tool.error {
                                            preview(
                                                &mut lines,
                                                &opencode::error_summary(Some(error)),
                                                width,
                                                theme.get("error"),
                                            );
                                        }
                                    }
                                    for row in &mut lines[start..] {
                                        row.background = card;
                                        row.rail = Some(theme.get("ui.window"));
                                    }
                                }
                            }
                            _ => (),
                        }
                    }
                    collapsed_reads(&mut lines, &mut reads, width, muted);
                    if let Some(error) = &message.error {
                        append(
                            &mut lines,
                            &opencode::error_summary(Some(error)),
                            width,
                            theme.get("error"),
                        );
                    }
                }
                "compaction" => append(&mut lines, "── Context compacted ──", width, muted),
                _ => continue,
            }
            for row in &mut lines[start..] {
                row.message = Some(message.id.clone());
            }
            lines.push(ChatRow::default());
        }
        for pending in &self.state.pending {
            let status = if pending.failed {
                "delivery uncertain"
            } else if pending.accepted {
                "queued"
            } else {
                "sending…"
            };
            self.user_block(
                &mut lines,
                &format!("You  ·  {status}"),
                &pending.text,
                width,
            );
            for snippet in &pending.snippets {
                append(&mut lines, &format!("▧ {}", snippet.label()), width, muted);
                if !snippet.comment.is_empty() {
                    append(
                        &mut lines,
                        &format!("  ↳ {}", snippet.comment),
                        width,
                        muted,
                    );
                }
            }
            if pending.snippets.is_empty() {
                for file in &pending.files {
                    let label = references::from_attachment(file)
                        .map(|target| target.label())
                        .unwrap_or_else(|| {
                            file.get("name")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("Attached file")
                                .to_owned()
                        });
                    append(&mut lines, &format!("▧ {label}"), width, muted);
                }
                render_selection_comments(&mut lines, pending.metadata.as_ref(), width, muted);
            }
            lines.push(ChatRow::default());
        }
        let mut notices: Vec<_> = self.state.notices.iter().collect();
        notices.sort_by_key(|(id, _)| *id);
        for (_, notice) in notices {
            append(
                &mut lines,
                &format!("! {notice}"),
                width,
                theme.get("warning"),
            );
        }
        if let Some(error) = &self.state.error {
            let start = lines.len();
            append(
                &mut lines,
                "Connection / execution error",
                width,
                theme.get("error").add_modifier(Modifier::BOLD),
            );
            append(&mut lines, error, width, theme.get("error"));
            for row in &mut lines[start..] {
                row.rail = Some(theme.get("error"));
            }
        }
        if self.state.busy {
            append(&mut lines, "⋯ Working…   c interrupt", width, muted);
        }
        // Keep the final row tight so auto-follow does not hide useful content.
        if lines.last().is_some_and(|row| row.spans.0.is_empty()) {
            lines.pop();
        }
        lines
    }

    fn user_block(&self, lines: &mut Vec<ChatRow>, label: &str, text: &str, width: usize) {
        let start = lines.len();
        lines.push(ChatRow::default());
        append(
            lines,
            label,
            width,
            self.theme.get("ui.text.focus").add_modifier(Modifier::BOLD),
        );
        append(lines, text, width, self.theme.get("ui.text"));
        lines.push(ChatRow::default());
        for row in &mut lines[start..] {
            row.background = self.theme.get("ui.popup");
            row.rail = Some(self.theme.get("ui.text.info"));
        }
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
        let theme = self.theme;
        fill(area, surface, theme.get("ui.background"));
        for y in area.y..area.bottom() {
            surface.set_stringn(area.x, y, "│", area.width as usize, theme.get("ui.window"));
        }
        if area.width < 6 || area.height == 0 {
            return;
        }
        let accent = theme.get("ui.text.info");
        let muted = theme.get("ui.text.inactive");
        let header = layout.header;
        let status = if !self.state.connected && self.state.status.starts_with("Connecting") {
            "◐ Connecting"
        } else if !self.state.connected {
            "○ Offline"
        } else if !self.state.notices.is_empty() {
            "◐ Needs input"
        } else if self.state.busy {
            "◐ Running"
        } else if self.state.error.is_some() {
            "× Error"
        } else {
            "● Ready"
        };
        let status_style = if self.state.error.is_some() {
            theme.get("error")
        } else if !self.state.notices.is_empty() {
            theme.get("warning")
        } else {
            accent
        };
        split_line(
            header,
            surface,
            "OpenCode",
            status,
            theme.get("ui.text").add_modifier(Modifier::BOLD),
            status_style,
        );
        if header.height >= 3 {
            let title = self
                .state
                .session
                .as_ref()
                .and_then(|session| {
                    session
                        .title
                        .as_deref()
                        .filter(|title| !title.is_empty())
                        .or_else(|| {
                            (!self.state.messages.is_empty()).then_some(session.id.as_str())
                        })
                })
                .unwrap_or("New conversation");
            write(
                header.clip_top(1).with_height(1),
                surface,
                title,
                theme.get("ui.text"),
            );
            let directory = self
                .state
                .session
                .as_ref()
                .map_or(self.directory, |session| {
                    session.location.directory.as_path()
                });
            let project = directory
                .file_name()
                .unwrap_or(directory.as_os_str())
                .to_string_lossy();
            split_line(
                header.clip_top(2).with_height(1),
                surface,
                &project,
                if follow { "" } else { "↑ history" },
                muted,
                accent,
            );
        }
        if lines.is_empty() && layout.body.height > 3 {
            let y = layout.body.y + layout.body.height.saturating_sub(6) / 2;
            let welcome = layout.body.clip_top(y - layout.body.y);
            write(
                welcome.with_height(1).clip_left(3),
                surface,
                "What would you like to work on?",
                accent.add_modifier(Modifier::BOLD),
            );
            if welcome.height >= 3 {
                write(
                    welcome.clip_top(2).with_height(1).clip_left(3),
                    surface,
                    "Ask about code, plan a change,",
                    muted,
                );
            }
            if welcome.height >= 4 {
                write(
                    welcome.clip_top(3).with_height(1).clip_left(3),
                    surface,
                    "or work through a problem.",
                    muted,
                );
            }
            if welcome.height >= 6 {
                write(
                    welcome.clip_top(5).with_height(1).clip_left(3),
                    surface,
                    "i  compose a message",
                    theme.get("ui.text"),
                );
            }
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
            // A small position indicator, without taking another transcript row.
            if !follow && layout.body.height > 0 && layout.body.width > 0 {
                let y = layout.body.y
                    + ((offset as u64 * layout.body.height.saturating_sub(1) as u64)
                        / lines.len().max(1) as u64) as u16;
                surface.set_string(layout.body.right() - 1, y, "▐", accent);
            }
        }
        let composer = layout.composer;
        fill(composer, surface, theme.get("ui.popup"));
        let border = if self.focused {
            accent
        } else {
            theme.get("ui.window")
        };
        for y in composer.y..composer.bottom() {
            surface.set_stringn(composer.x, y, "┃", composer.width as usize, border);
        }
        let content = composer.clip_left(2).clip_right(1);
        if content.height > 0 {
            let title = if self.snippets.is_empty() {
                if self.editing {
                    "Message  ·  editing".to_owned()
                } else if self.draft.is_empty() {
                    "Message".to_owned()
                } else {
                    "Message  ·  draft".to_owned()
                }
            } else {
                let comments = self
                    .snippets
                    .iter()
                    .filter(|snippet| !snippet.comment.is_empty())
                    .count();
                format!(
                    "Message · {} snippet(s), {comments} comment(s): {}",
                    self.snippets.len(),
                    self.snippets[0].label()
                )
            };
            write(content.with_height(1), surface, &title, muted);
        }
        if content.height > 2 && !self.editing {
            let mut draft_lines = Vec::new();
            append(
                &mut draft_lines,
                if self.draft.is_empty() {
                    if !self.state.connected {
                        "r  connect to OpenCode"
                    } else if self.state.busy {
                        "Steer the conversation…"
                    } else {
                        "Ask OpenCode…  (i / Enter)"
                    }
                } else {
                    self.draft
                },
                content.width as usize,
                if self.draft.is_empty() {
                    muted
                } else {
                    theme.get("ui.text")
                },
            );
            for (index, row) in draft_lines
                .iter()
                .take(content.height.saturating_sub(2) as usize)
                .enumerate()
            {
                surface.set_spans(
                    content.x,
                    content.y + 1 + index as u16,
                    &row.spans,
                    content.width,
                );
            }
        }
        if content.height > 1 {
            let session = self.state.session.as_ref();
            let agent = session
                .and_then(|s| s.agent.as_deref())
                .unwrap_or("Default agent");
            let model = session
                .and_then(|s| s.model.as_ref())
                .map_or_else(|| "Server default".to_owned(), |m| m.label());
            split_line(
                Rect::new(content.x, content.bottom() - 1, content.width, 1),
                surface,
                agent,
                &model,
                accent,
                muted,
            );
        }
        let hints = if self.editing {
            "Enter send   Esc cancel"
        } else if self.state.busy {
            "i steer   c interrupt   q close"
        } else if self.expanded {
            "i message   t hide details   n new"
        } else {
            "i message   f files   ]/[ refs   J/K chat"
        };
        write(layout.footer, surface, hints, muted);
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
    let mut rows = Vec::new();
    append(&mut rows, text, area.width as usize, style);
    if let Some(row) = rows.first() {
        surface.set_spans(area.x, area.y, &row.spans, area.width);
    }
}

fn split_line(
    area: Rect,
    surface: &mut Buffer,
    left: &str,
    right: &str,
    left_style: Style,
    right_style: Style,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let right_width = right.width().min(area.width as usize) as u16;
    let left_width = area.width.saturating_sub(right_width.saturating_add(2));
    // At narrow widths, keep the primary label rather than the secondary one.
    if left_width < 8 {
        write(area, surface, left, left_style);
    } else {
        write(area.with_width(left_width), surface, left, left_style);
        write(
            Rect::new(area.right() - right_width, area.y, right_width, 1),
            surface,
            right,
            right_style,
        );
    }
}

fn append(lines: &mut Vec<ChatRow>, text: &str, width: usize, style: Style) {
    for line in text.replace('\t', "    ").split('\n') {
        wrap_spans(lines, &Spans::from(Span::styled(line, style)), width);
    }
}

fn render_selection_comments(
    lines: &mut Vec<ChatRow>,
    metadata: Option<&serde_json::Value>,
    width: usize,
    style: Style,
) {
    let Some(comments) = metadata
        .and_then(|m| m.get("comments"))
        .and_then(serde_json::Value::as_array)
    else {
        return;
    };
    for item in comments {
        let Some(comment) = item
            .get("comment")
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.trim().is_empty())
        else {
            continue;
        };
        let path = item
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Quoted selection");
        let line = item
            .get("selection")
            .and_then(|s| s.get("startLine"))
            .and_then(serde_json::Value::as_u64);
        let label = line.map_or_else(|| path.to_owned(), |line| format!("{path}:{line}"));
        append(lines, &format!("  ↳ {label}: {comment}"), width, style);
    }
}

fn collapsed_reads(lines: &mut Vec<ChatRow>, count: &mut usize, width: usize, style: Style) {
    if *count > 0 {
        append(
            lines,
            &format!(
                "▸ Read {} file{}  ·  t details",
                count,
                if *count == 1 { "" } else { "s" }
            ),
            width,
            style,
        );
        *count = 0;
    }
}

/// Preserve inline Markdown styling when wrapping, and never pass terminal
/// control sequences or an over-wide glyph into the compositor.
fn wrap_spans(lines: &mut Vec<ChatRow>, spans: &Spans<'_>, width: usize) {
    wrap_linked_spans(lines, spans, &[], width);
}

fn wrap_linked_spans(
    lines: &mut Vec<ChatRow>,
    spans: &Spans<'_>,
    links: &[Option<references::Reference>],
    width: usize,
) {
    if width == 0 {
        return;
    }
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_links = Vec::new();
    let mut used = 0;
    for (span_index, span) in spans.0.iter().enumerate() {
        let reference = links.get(span_index).and_then(Option::as_ref);
        for word in span.content.split_word_bounds() {
            if used > 0 && word.width() <= width && used + word.width() > width {
                lines.push(ChatRow {
                    spans: Spans::from(std::mem::take(&mut current)),
                    links: std::mem::take(&mut current_links),
                    ..ChatRow::default()
                });
                used = 0;
                if word.chars().all(char::is_whitespace) {
                    continue;
                }
            }
            for grapheme in word.graphemes(true) {
                let size = grapheme.width();
                if grapheme.chars().any(char::is_control) || size > width {
                    continue;
                }
                if used + size > width {
                    lines.push(ChatRow {
                        spans: Spans::from(std::mem::take(&mut current)),
                        links: std::mem::take(&mut current_links),
                        ..ChatRow::default()
                    });
                    used = 0;
                }
                let last_reference = current_links
                    .last()
                    .filter(|(index, _)| *index + 1 == current.len())
                    .map(|(_, reference)| reference);
                if let Some(last) = current
                    .last_mut()
                    .filter(|last| last.style == span.style && last_reference == reference)
                {
                    last.content.to_mut().push_str(grapheme);
                } else {
                    if let Some(reference) = reference {
                        current_links.push((current.len(), reference.clone()));
                    }
                    current.push(Span::styled(grapheme.to_owned(), span.style));
                }
                used += size;
            }
        }
    }
    lines.push(ChatRow {
        spans: Spans::from(current),
        links: current_links,
        ..ChatRow::default()
    });
}

fn preview(lines: &mut Vec<ChatRow>, text: &str, width: usize, style: Style) {
    let mut rows = Vec::new();
    // Bound tool output even if a service returns a very large result.
    let snippet: String = text.chars().take(2048).collect();
    append(&mut rows, &snippet, width, style);
    let truncated = rows.len() > 8 || snippet.len() < text.len();
    lines.extend(rows.into_iter().take(8));
    if truncated {
        append(lines, "… preview truncated", width, style);
    }
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

    fn view<'a>(state: &'a State, loader: &'a Arc<ArcSwap<syntax::Loader>>) -> ChatView<'a> {
        ChatView {
            state,
            loader,
            draft: "",
            directory: std::path::Path::new("/work/helix"),
            theme: &helix_view::theme::DEFAULT_THEME,
            expanded: false,
            focused: true,
            editing: false,
            snippets: &[],
        }
    }

    fn row_text(row: &ChatRow) -> String {
        row.spans
            .0
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
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
    fn transcript_has_roles_wraps_unicode_and_keeps_errors_visible() {
        let mut state = State::default();
        state.messages.push(Message {
            id: "msg_user".into(),
            kind: "user".into(),
            text: "hello 世界\nnext line".into(),
            ..Message::default()
        });
        state.error = Some("connection failed".into());
        let theme = &helix_view::theme::DEFAULT_THEME;
        let loader = loader();
        let lines = view(&state, &loader).transcript(8);
        assert!(lines.iter().any(|row| row_text(row) == "You"));
        assert!(lines.iter().all(|row| row.spans.width() <= 8));
        assert!(lines.iter().any(|row| row
            .spans
            .0
            .iter()
            .any(|span| span.style == theme.get("error"))));
        let mut narrow = Vec::new();
        append(&mut narrow, "世a\u{1b}", 1, Style::default());
        assert!(narrow
            .iter()
            .all(|row| row.spans.width() <= 1 && !row_text(row).contains('\u{1b}')));
    }

    #[test]
    fn markdown_preserves_emphasis_lists_and_code_without_fences() {
        let mut state = State::default();
        state.messages.push(Message {
            kind: "assistant".into(),
            content: vec![part(serde_json::json!({"type":"text", "text":"## Summary\n\n**Important** and `code`.\n\n- First\n- Second\n\n```rust\nlet answer = 42;\n```"}))],
            ..Message::default()
        });
        let loader = loader();
        let rows = view(&state, &loader).transcript(40);
        let output = rows.iter().map(row_text).collect::<Vec<_>>().join("\n");
        assert!(output.contains("Summary"));
        assert!(output.contains("• First"));
        assert!(output.contains("let answer = 42;"));
        assert!(!output.contains("```"));
        assert!(!output.contains("**Important**"));
        assert!(rows
            .iter()
            .flat_map(|row| &row.spans.0)
            .any(|span| span.content == "Important"
                && span.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn details_expand_reasoning_and_bounded_tool_previews() {
        let mut state = State::default();
        state.messages.push(Message {
            kind: "assistant".into(),
            content: vec![
                part(serde_json::json!({"type":"reasoning", "text":"Inspect the failing connection"})),
                part(serde_json::json!({"type":"tool", "name":"read", "state":{
                    "status":"completed", "input":{"path":"src/main.rs"},
                    "content":[{"type":"text","text":"output line\n".repeat(1000)}]
                }})),
            ], ..Message::default()
        });
        let loader = loader();
        let mut view = view(&state, &loader);
        let compact = view
            .transcript(50)
            .iter()
            .map(row_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(compact.contains("▸ Read 1 file"));
        assert!(!compact.contains("src/main.rs"));
        assert!(!compact.contains("Inspect the failing"));
        assert!(!compact.contains("output line"));
        view.expanded = true;
        let rows = view.transcript(50);
        let expanded = rows.iter().map(row_text).collect::<Vec<_>>().join("\n");
        assert!(expanded.contains("Inspect the failing"));
        assert!(expanded.contains("✓ read  src/main.rs"));
        assert!(expanded.contains("output line"));
        assert!(expanded.contains("preview truncated"));
        assert!(rows.len() < 30);
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
        let loader = loader();
        let output = view(&state, &loader)
            .transcript(160)
            .iter()
            .map(row_text)
            .collect::<Vec<_>>()
            .join("\n");
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
        state.messages.push(Message {
            id: "msg_links".into(), kind: "assistant".into(),
            content: vec![part(serde_json::json!({"type":"text","text":
                "See [**é界 implementation**](src/first.rs#L12C3) and [other](src/second.rs:8).\n\n`src/third.rs:4` and src/fourth.rs:9."
            }))], ..Message::default()
        });
        let loader = loader();
        for width in [8, 20, 200] {
            let rows = view(&state, &loader).transcript(width);
            let rendered = rows.iter().map(row_text).collect::<Vec<_>>().join("\n");
            assert!(!rendered.contains("↳"));
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
            if width == 200 {
                assert!(rows.iter().any(|row| {
                    row.links
                        .iter()
                        .any(|(_, r)| r.target.path.to_str() == Some("src/first.rs"))
                        && row
                            .links
                            .iter()
                            .any(|(_, r)| r.target.path.to_str() == Some("src/second.rs"))
                }));
            }
        }
    }

    #[test]
    fn web_image_labels_and_code_blocks_are_not_local_links() {
        let mut state = State::default();
        state.messages.push(Message {
            kind: "assistant".into(),
            content: vec![part(serde_json::json!({"type":"text","text":
                "[src/web.rs](https://example.com) ![src/image.rs](image.png)\n\n```rust\nsrc/not-a-link.rs\n```\n\n[local](src/local.rs)"
            }))], ..Message::default()
        });
        let loader = loader();
        let rows = view(&state, &loader).transcript(100);
        let links: Vec<_> = rows.iter().flat_map(|row| &row.links).collect();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1.target.path.to_str(), Some("src/local.rs"));
    }

    #[test]
    fn consecutive_completed_reads_collapse_but_running_and_failed_tools_stay_visible() {
        let mut state = State::default();
        state.messages.push(Message {
            kind: "assistant".into(),
            content: vec![
                part(serde_json::json!({"type":"tool","name":"opencode.read","state":{"status":"completed","input":{"path":"one.rs"}}})),
                part(serde_json::json!({"type":"tool","name":"read","state":{"status":"completed","input":{"path":"two.rs"}}})),
                part(serde_json::json!({"type":"tool","name":"read","state":{"status":"running","input":{"path":"running.rs"}}})),
                part(serde_json::json!({"type":"tool","name":"read","state":{"status":"error","input":{"path":"failed.rs"}}})),
                part(serde_json::json!({"type":"text","text":"Done."})),
            ], ..Message::default()
        });
        let loader = loader();
        let mut view = view(&state, &loader);
        let compact = view
            .transcript(100)
            .iter()
            .map(row_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(compact.contains("▸ Read 2 files"));
        assert!(!compact.contains("one.rs"));
        assert!(!compact.contains("two.rs"));
        assert!(compact.contains("running.rs"));
        assert!(compact.contains("failed.rs"));
        assert_eq!(
            references::for_message(&state.messages[0], std::path::Path::new("/work")).len(),
            4
        );
        view.expanded = true;
        let expanded = view
            .transcript(100)
            .iter()
            .map(row_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!expanded.contains("▸ Read"));
        assert!(expanded.contains("one.rs"));
        assert!(expanded.contains("two.rs"));
    }

    #[test]
    fn empty_state_and_composer_are_persistent_and_show_drafts() {
        let mut state = State::default();
        state.connected = true;
        let loader = loader();
        let mut view = view(&state, &loader);
        let area = Rect::new(0, 0, 64, 24);
        let layout = ChatLayout::new(area);
        let mut surface = Buffer::empty(area);
        view.render(area, &mut surface, &layout, &[], 0, true);
        let output = screen(&surface);
        assert!(output.contains("OpenCode"));
        assert!(output.contains("● Ready"));
        assert!(output.contains("What would you like"));
        assert!(output.contains("Ask OpenCode"));
        view.draft = "Review the current changes";
        view.render(area, &mut surface, &layout, &[], 0, true);
        let output = screen(&surface);
        assert!(output.contains("Message  ·  draft"));
        assert!(output.contains("Review the current changes"));
        assert!(!output.contains("Ask OpenCode"));
        view.editing = true;
        view.render(area, &mut surface, &layout, &[], 0, true);
        let output = screen(&surface);
        assert!(output.contains("Message  ·  editing"));
        assert!(output.contains("Enter send   Esc cancel"));
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
    fn small_and_offset_panes_never_draw_outside_their_area() {
        let mut state = State::default();
        state.error = Some("Network unavailable".into());
        let loader = loader();
        let view = view(&state, &loader);
        for width in [0, 1, 2, 5, 8, 20, 64] {
            for height in [0, 1, 2, 4, 8, 12, 24] {
                let area = Rect::new(2, 2, width, height);
                let outer = Rect::new(0, 0, width + 4, height + 4);
                let mut surface = Buffer::empty(outer);
                for y in 0..outer.height {
                    surface.set_string(0, y, &"x".repeat(outer.width as usize), Style::default());
                }
                let layout = ChatLayout::new(area);
                let rows = view.transcript(layout.body.width.saturating_sub(3) as usize);
                view.render(area, &mut surface, &layout, &rows, 0, true);
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
