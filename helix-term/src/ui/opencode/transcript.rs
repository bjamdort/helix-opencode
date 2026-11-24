//! Transcript rows modeled on the OpenCode TUI session view: an indented
//! conversation, shaded user and tool blocks, collapsible activity summaries,
//! and a per-turn footer.
use super::*;
use crate::opencode::{Message, Part, ToolState};
use serde_json::Value;
use std::path::Path;

/// Content column for assistant rows, matching the OpenCode TUI.
pub(super) const INDENT: u16 = 3;
/// Content column inside shaded blocks: indent, left edge, and padding.
const BLOCK_INDENT: u16 = INDENT + 3;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Diff and file previews are bounded inside the pane.
const PREVIEW_LINES: usize = 40;
const SHELL_LINES: usize = 10;

pub(super) fn spinner_frame() -> &'static str {
    let tick = opencode::now_millis() / opencode::SPINNER_INTERVAL_MS;
    SPINNER[(tick % SPINNER.len() as u64) as usize]
}

/// Theme roles used by the chat pane. Each has an optional `ui.opencode.*`
/// override and falls back to a scope that common Helix themes define.
pub(super) struct Palette {
    pub page: Style,
    pub text: Style,
    pub muted: Style,
    pub accent: Style,
    pub agent: Style,
    pub panel: Style,
    pub prompt: Style,
    pub border: Style,
    pub error: Style,
    pub warning: Style,
    pub info: Style,
    pub thinking: Style,
    pub diff_plus: Style,
    pub diff_minus: Style,
    pub diff_hunk: Style,
}

fn background(style: Style) -> Style {
    style
        .bg
        .map_or_else(Style::default, |color| Style::default().bg(color))
}

impl Palette {
    pub fn new(theme: &Theme) -> Self {
        let accent = chat_style(theme, "accent", "keyword");
        let panel = background(chat_style(theme, "panel", "ui.popup"));
        let prompt = background(chat_style(theme, "prompt", "ui.popup"));
        Self {
            page: theme.get("ui.background"),
            text: chat_style(theme, "text", "ui.text"),
            muted: chat_style(theme, "muted", "ui.text.inactive"),
            agent: theme.try_get_exact("ui.opencode.agent").unwrap_or(accent),
            accent,
            panel,
            prompt: if prompt.bg.is_some() { prompt } else { panel },
            border: chat_style(theme, "border", "ui.window"),
            error: theme.get("error"),
            warning: theme.get("warning"),
            info: chat_style(theme, "info", "info"),
            thinking: chat_style(theme, "thinking", "warning"),
            diff_plus: theme.get("diff.plus"),
            diff_minus: theme.get("diff.minus"),
            diff_hunk: theme.get("diff.delta"),
        }
    }
}

#[derive(Default)]
pub(super) struct ChatRow {
    pub spans: Spans<'static>,
    /// Text column, relative to the transcript.
    pub indent: u16,
    /// Background from a column to the right edge.
    pub fill: Option<(u16, Style)>,
    /// A `┃` edge at a column.
    pub rail: Option<(u16, Style)>,
    pub links: Vec<(usize, references::Reference)>,
    pub message: Option<String>,
}

impl ChatRow {
    pub fn render(&self, area: Rect, surface: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if let Some((column, style)) = self.fill {
            let fill = area.clip_left(column);
            if fill.width > 0 {
                surface.set_style(fill, style);
            }
        }
        if let Some((column, style)) = self.rail {
            if column < area.width {
                surface.set_stringn(area.x + column, area.y, "┃", 1, style);
            }
        }
        let text = area.clip_left(self.indent);
        if text.width > 0 {
            surface.set_spans(text.x, text.y, &self.spans, text.width);
        }
    }

    fn is_blank(&self) -> bool {
        self.spans.width() == 0 && self.fill.is_none() && self.rail.is_none()
    }
}

pub(super) struct Transcript<'a> {
    pub lines: Vec<ChatRow>,
    width: usize,
    palette: &'a Palette,
    theme: &'a Theme,
    loader: &'a Arc<ArcSwap<syntax::Loader>>,
    directory: &'a Path,
    expanded: bool,
    /// Rows are stamped with their message for message navigation.
    message: Option<String>,
}

/// A run of reasoning and tool parts, possibly across steps, summarized as one row.
type Activity<'m> = Vec<(&'m Message, &'m Part)>;

pub(super) struct TranscriptInput<'a> {
    pub state: &'a State,
    pub palette: &'a Palette,
    pub theme: &'a Theme,
    pub loader: &'a Arc<ArcSwap<syntax::Loader>>,
    pub directory: &'a Path,
    pub expanded: bool,
}

pub(super) fn build(input: TranscriptInput<'_>, width: usize) -> Vec<ChatRow> {
    let state = input.state;
    let mut transcript = Transcript {
        lines: Vec::new(),
        width,
        palette: input.palette,
        theme: input.theme,
        loader: input.loader,
        directory: input.directory,
        expanded: input.expanded,
        message: None,
    };
    let legacy = !state.messages.iter().any(|message| message.kind == "idle");
    let mut activity: Activity = Vec::new();
    let mut last_error: Option<String> = None;
    for (index, message) in state.messages.iter().enumerate() {
        if message.kind != "assistant" {
            transcript.activity(&mut activity, true);
        }
        transcript.message = Some(message.id.clone());
        match message.kind.as_str() {
            "user" => transcript.user_message(message),
            "assistant" => {
                for part in &message.content {
                    match part.kind.as_str() {
                        "text" if !part.text.trim().is_empty() => {
                            transcript.activity(&mut activity, true);
                            transcript.message = Some(message.id.clone());
                            transcript.gap();
                            transcript.markdown(&message.id, &part.text, INDENT, None);
                        }
                        "tool" if canonical(&part.name) == "question" => {
                            transcript.activity(&mut activity, true);
                            transcript.message = Some(message.id.clone());
                            transcript.tool(message, part);
                        }
                        "tool" | "reasoning" => activity.push((message, part)),
                        _ => (),
                    }
                }
                if has_footer(message) {
                    transcript.activity(&mut activity, true);
                    transcript.message = Some(message.id.clone());
                    last_error = message.error.as_ref().map(|e| error_text(Some(e)));
                    let duration = turn_duration(&state.messages, index, legacy);
                    transcript.footer(message, duration);
                }
            }
            "compaction" => transcript.compaction(message),
            "agent-switched" | "model-switched" => transcript.switched(message),
            "location-switched" => transcript.moved(message),
            "system" | "synthetic" => transcript.notice(message),
            "skill" => {
                let name = message.name.as_deref().unwrap_or("skill");
                transcript.gap();
                transcript.inline("→", &[(format!("Skill {name}"), None)], input.palette.muted);
            }
            "shell" => transcript.shell_message(message),
            "idle" => {
                let error = message.error.as_ref().map(|error| error_text(Some(error)));
                if let Some(error) = error.filter(|error| last_error.as_ref() != Some(error)) {
                    transcript.gap();
                    transcript.text(INDENT, &format!("Error: {error}"), input.palette.error);
                }
                last_error = None;
            }
            _ => (),
        }
    }
    transcript.activity(&mut activity, !state.busy);
    transcript.message = None;
    for pending in &state.pending {
        transcript.pending(pending);
    }
    if let Some(error) = &state.error {
        transcript.gap();
        transcript.text(INDENT, &format!("Error: {error}"), input.palette.error);
    }
    transcript.lines
}

fn canonical(name: &str) -> &str {
    match name.rsplit('.').next().unwrap_or(name) {
        "bash" => "shell",
        "task" => "subagent",
        "apply_patch" => "patch",
        name => name,
    }
}

fn has_footer(message: &Message) -> bool {
    message.error.is_some()
        || message
            .finish
            .as_deref()
            .is_some_and(|finish| finish != "tool-calls" && finish != "unknown")
}

fn error_text(error: Option<&Value>) -> String {
    opencode::error_summary(error)
}

/// Milliseconds from the prompt that started the turn until this step completed.
fn turn_duration(messages: &[Message], index: usize, legacy: bool) -> Option<u64> {
    let message = &messages[index];
    let completed = message.time.completed?;
    let mut input = None;
    for candidate in messages[..index].iter().rev() {
        if candidate.kind == "idle" {
            break;
        }
        if candidate.kind == "user" || candidate.kind == "synthetic" {
            input = Some(candidate);
            if legacy {
                break;
            }
        }
    }
    let start = input
        .and_then(|input| input.time.created)
        .or(message.time.created)?;
    Some(completed.saturating_sub(start))
}

pub(super) fn duration(ms: u64) -> String {
    match ms {
        0..1_000 => format!("{ms}ms"),
        1_000..60_000 => format!("{:.1}s", ms as f64 / 1000.0),
        60_000..3_600_000 => format!("{}m {}s", ms / 60_000, ms % 60_000 / 1000),
        3_600_000..86_400_000 => format!("{}h {}m", ms / 3_600_000, ms % 3_600_000 / 60_000),
        _ => format!("{}d {}h", ms / 86_400_000, ms % 86_400_000 / 3_600_000),
    }
}

pub(super) fn number(value: f64) -> String {
    if value >= 999_950.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value >= 1000.0 {
        format!("{:.1}K", value / 1000.0)
    } else {
        format!("{}", value.round() as u64)
    }
}

pub(super) fn titlecase(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn string<'v>(value: Option<&'v Value>, key: &str) -> Option<&'v str> {
    value
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn integer(value: Option<&Value>, key: &str) -> Option<u64> {
    value
        .and_then(|value| value.get(key))
        .and_then(Value::as_u64)
}

fn strip_ansi(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            output.push(c);
            continue;
        }
        if chars.next_if_eq(&'[').is_some() {
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else if chars.next_if_eq(&']').is_some() {
            while let Some(next) = chars.next() {
                if next == '\u{7}' || (next == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    output
}

/// The leading `**Title**` paragraph of a thought, as the TUI shows it.
fn reasoning_summary(text: &str) -> (Option<&str>, &str) {
    let content = text.trim();
    if let Some(rest) = content.strip_prefix("**") {
        if let Some(end) = rest.find("**") {
            let title = &rest[..end];
            let after = &rest[end + 2..];
            if !title.is_empty() && !title.contains('\n') && !title.contains('*') {
                if after.is_empty() {
                    return (Some(title.trim()), "");
                }
                if let Some(body) = after
                    .strip_prefix("\n\n")
                    .or_else(|| after.strip_prefix("\r\n\r\n"))
                {
                    return (Some(title.trim()), body.trim_end());
                }
            }
        }
    }
    (None, content)
}

fn reasoning_text(part: &Part) -> String {
    part.text.replace("[REDACTED]", "").trim().to_owned()
}

struct Tool<'p> {
    name: &'p str,
    status: &'p str,
    input: Option<&'p Value>,
    metadata: Option<&'p Value>,
    error: Option<String>,
}

impl<'p> Tool<'p> {
    fn new(part: &'p Part) -> Self {
        let state: Option<&ToolState> = part.state.as_ref();
        let status = state.map_or("streaming", |state| state.status.as_str());
        Self {
            name: canonical(&part.name),
            status: if status.is_empty() {
                "streaming"
            } else {
                status
            },
            input: state
                .and_then(|state| state.input.as_ref())
                .filter(|input| input.is_object()),
            metadata: state.and_then(|state| state.metadata.as_ref()),
            error: state
                .and_then(|state| state.error.as_ref())
                .map(|error| error_text(Some(error))),
        }
    }

    fn active(&self) -> bool {
        matches!(self.status, "streaming" | "running")
    }

    fn failed(&self) -> bool {
        self.status == "error"
    }

    fn denied(&self) -> bool {
        self.error.as_deref().is_some_and(|error| {
            [
                "rejected permission",
                "QuestionRejectedError",
                "specified a rule",
                "user dismissed",
            ]
            .iter()
            .any(|needle| error.contains(needle))
        })
    }

    fn input(&self, key: &str) -> Option<&'p str> {
        string(self.input, key)
    }

    fn path(&self) -> Option<&'p str> {
        ["path", "filePath", "file_path", "filename"]
            .iter()
            .find_map(|key| self.input(key))
    }

    fn output(state: Option<&ToolState>) -> String {
        state
            .map(|state| {
                state
                    .content
                    .iter()
                    .filter_map(|content| {
                        content
                            .get("text")
                            .or_else(|| content.get("name"))
                            .or_else(|| content.get("uri"))
                            .and_then(Value::as_str)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    /// The status label for running work, independent of streamed details.
    fn busy_label(&self) -> String {
        let noun = match self.name {
            "shell" => "command",
            "execute" => "code",
            name => name,
        };
        let verb = if self.status == "streaming" {
            "Preparing"
        } else {
            "Running"
        };
        format!("{verb} {noun}…")
    }
}

fn reasoning_active(message: &Message, part: &Part, closed: bool) -> bool {
    !closed && !part.is_complete() && message.time.completed.is_none()
}

/// "3 commands, 1 edit, 2 thoughts, 4 reads", counting finished work only.
fn activity_summary(items: &Activity, closed: bool) -> (String, Option<String>) {
    let mut counts = [0usize; 5];
    let mut busy = None;
    for (message, part) in items {
        if part.kind == "reasoning" {
            if reasoning_active(message, part, closed) {
                busy.get_or_insert_with(|| "Thinking…".to_owned());
            } else if !reasoning_text(part).is_empty() {
                counts[2] += 1;
            }
            continue;
        }
        let tool = Tool::new(part);
        if tool.name == "execute" {
            let calls: Vec<_> = tool
                .metadata
                .and_then(|metadata| metadata.get("toolCalls"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|call| string(Some(call), "status").is_some_and(|s| s != "running"))
                .collect();
            for call in &calls {
                if string(Some(call), "tool").map(canonical) == Some("read") {
                    counts[3] += 1;
                } else {
                    counts[4] += 1;
                }
            }
            if calls.is_empty() && !tool.active() {
                counts[4] += 1;
            }
            if tool.active() {
                busy.get_or_insert_with(|| tool.busy_label());
            }
            continue;
        }
        if tool.active() {
            busy.get_or_insert_with(|| tool.busy_label());
            continue;
        }
        let slot = match tool.name {
            "shell" => 0,
            "edit" | "write" | "patch" => 1,
            "read" => 3,
            _ => 4,
        };
        counts[slot] += 1;
    }
    let label = ["command", "edit", "thought", "read", "tool"]
        .iter()
        .zip(counts)
        .filter(|(_, count)| *count > 0)
        .map(|(name, count)| format!("{count} {name}{}", if count == 1 { "" } else { "s" }))
        .collect::<Vec<_>>()
        .join(", ");
    (label, busy)
}

impl Transcript<'_> {
    fn content_width(&self, indent: u16, right: u16) -> usize {
        self.width
            .saturating_sub(indent as usize)
            .saturating_sub(right as usize)
    }

    /// One blank row between transcript items, like the TUI's `marginTop={1}`.
    fn gap(&mut self) {
        if self.lines.last().is_some_and(|row| !row.is_blank()) {
            self.blank();
        }
    }

    fn blank(&mut self) {
        self.lines.push(ChatRow {
            message: self.message.clone(),
            ..ChatRow::default()
        });
    }

    fn push(
        &mut self,
        indent: u16,
        right: u16,
        spans: &Spans<'_>,
        links: &[Option<references::Reference>],
        hang: usize,
    ) {
        let start = self.lines.len();
        let width = self.content_width(indent, right);
        if width == 0 {
            return;
        }
        wrap(&mut self.lines, spans, links, width, hang);
        for row in &mut self.lines[start..] {
            row.indent = indent;
            row.message = self.message.clone();
        }
    }

    fn text(&mut self, indent: u16, text: &str, style: Style) {
        self.text_with(indent, 1, text, style);
    }

    fn text_with(&mut self, indent: u16, right: u16, text: &str, style: Style) {
        for line in text.replace('\t', "    ").split('\n') {
            self.push(
                indent,
                right,
                &Spans::from(Span::styled(line, style)),
                &[],
                0,
            );
        }
    }

    fn reference(&self, message: &Message, path: &str) -> Option<references::Reference> {
        references::parse(path, true).map(|target| references::Reference {
            message_id: message.id.clone(),
            directory: self.session_directory(message).to_owned(),
            target,
        })
    }

    fn session_directory<'m>(&'m self, message: &'m Message) -> &'m Path {
        message
            .location
            .as_ref()
            .map_or(self.directory, |location| location.directory.as_path())
    }

    /// Paths relative to the session directory, as the TUI's path formatter shows them.
    fn display_path(&self, path: &str) -> String {
        Path::new(path)
            .strip_prefix(self.directory)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map_or_else(|| path.to_owned(), |p| p.to_string_lossy().into_owned())
    }

    fn markdown(&mut self, message_id: &str, text: &str, indent: u16, tint: Option<Style>) {
        let markdown =
            crate::ui::markdown::Markdown::new(text.to_owned(), self.loader.clone()).for_chat();
        let (rendered, inline) = markdown.parse_with_inline(Some(self.theme));
        let directory = self.directory.to_owned();
        let mut rows = rendered.lines.len();
        // Markdown ends lists and code blocks with an empty line.
        while rows > 0 && rendered.lines[rows - 1].width() == 0 {
            rows -= 1;
        }
        for (line_index, line) in rendered.lines.iter().take(rows).enumerate() {
            let mut spans = Vec::new();
            let mut links = Vec::new();
            for (span_index, span) in line.0.iter().enumerate() {
                let style = match tint {
                    Some(tint) => Style {
                        fg: tint.fg,
                        ..span.style
                    },
                    None => span.style,
                };
                let targets = inline
                    .iter()
                    .find(|item| item.line == line_index && item.span == span_index)
                    .filter(|_| tint.is_none())
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
                            style,
                        ));
                        links.push(None);
                    }
                    spans.push(Span::styled(
                        span.content[range.clone()].to_owned(),
                        style.underline_style(UnderlineStyle::Line),
                    ));
                    links.push(Some(references::Reference {
                        message_id: message_id.to_owned(),
                        directory: directory.clone(),
                        target,
                    }));
                    offset = range.end;
                }
                if offset < span.content.len() {
                    spans.push(Span::styled(span.content[offset..].to_owned(), style));
                    links.push(None);
                }
            }
            self.push(indent, 1, &Spans::from(spans), &links, 0);
        }
    }

    fn user_message(&mut self, message: &Message) {
        let display = message
            .metadata
            .as_ref()
            .and_then(|m| m.get("displayText"))
            .and_then(Value::as_str)
            .unwrap_or(&message.text);
        let chips = message
            .files
            .iter()
            .map(|file| {
                let reference =
                    references::from_attachment(file).map(|target| references::Reference {
                        message_id: message.id.clone(),
                        directory: self.session_directory(message).to_owned(),
                        target,
                    });
                let label = reference.as_ref().map_or_else(
                    || {
                        string(Some(file), "name")
                            .unwrap_or("Attached file")
                            .to_owned()
                    },
                    |reference| reference.target.label(),
                );
                (label, reference)
            })
            .collect();
        let comments = selection_comments(message.metadata.as_ref());
        self.user_block(display, chips, &comments, None, self.palette.agent);
    }

    fn pending(&mut self, pending: &opencode::Pending) {
        let status = if pending.failed {
            "Delivery uncertain"
        } else if pending.accepted {
            "Queued"
        } else {
            "Sending…"
        };
        let mut chips: Vec<(String, Option<references::Reference>)> = pending
            .snippets
            .iter()
            .map(|snippet| (snippet.label(), None))
            .collect();
        let mut comments: Vec<String> = pending
            .snippets
            .iter()
            .filter(|snippet| !snippet.comment.is_empty())
            .map(|snippet| format!("{}: {}", snippet.label(), snippet.comment))
            .collect();
        if pending.snippets.is_empty() {
            chips = pending
                .files
                .iter()
                .map(|file| {
                    let label = references::from_attachment(file).map_or_else(
                        || {
                            string(Some(file), "name")
                                .unwrap_or("Attached file")
                                .to_owned()
                        },
                        |target| target.label(),
                    );
                    (label, None)
                })
                .collect();
            comments = selection_comments(pending.metadata.as_ref());
        }
        self.user_block(
            &pending.text,
            chips,
            &comments,
            Some(status),
            self.palette.border,
        );
    }

    fn user_block(
        &mut self,
        text: &str,
        chips: Vec<(String, Option<references::Reference>)>,
        comments: &[String],
        status: Option<&str>,
        bar: Style,
    ) {
        self.gap();
        let start = self.lines.len();
        self.blank();
        if !text.trim().is_empty() {
            self.text_with(INDENT, 2, text, self.palette.text);
        }
        if !chips.is_empty() {
            if !text.trim().is_empty() {
                self.blank();
            }
            let chip = Style {
                fg: self.palette.page.bg,
                bg: self.palette.accent.fg,
                ..Style::default()
            }
            .add_modifier(Modifier::BOLD);
            let name = self.palette.prompt.patch(Style {
                fg: self.palette.muted.fg,
                ..Style::default()
            });
            for (label, reference) in chips {
                let spans = Spans::from(vec![
                    Span::styled(" file ", chip),
                    Span::styled(format!(" {label} "), name),
                ]);
                self.push(INDENT, 2, &spans, &[None, reference], 0);
            }
        }
        for comment in comments {
            self.text_with(INDENT, 2, &format!("↳ {comment}"), self.palette.muted);
        }
        if let Some(status) = status {
            self.text_with(INDENT, 2, status, self.palette.muted);
        }
        self.blank();
        for row in &mut self.lines[start..] {
            row.fill = Some((0, self.palette.panel));
            row.rail = Some((0, bar));
        }
    }

    /// `+ 3 commands, 1 edit` collapsed, or `− …` followed by each item.
    fn activity(&mut self, items: &mut Activity, closed: bool) {
        if items.is_empty() {
            return;
        }
        let group = std::mem::take(items);
        let (label, busy) = activity_summary(&group, closed);
        let label = if label.is_empty() {
            busy.clone().unwrap_or_default()
        } else {
            label
        };
        let failed = group
            .iter()
            .filter(|(_, part)| part.kind == "tool")
            .any(|(_, part)| Tool::new(part).failed());
        if label.is_empty() && !failed {
            return;
        }
        self.message = Some(group[0].0.id.clone());
        self.gap();
        let icon = if self.expanded {
            "−"
        } else if busy.is_some() {
            spinner_frame()
        } else {
            "+"
        };
        self.inline(icon, &[(label, None)], self.palette.muted);
        if !self.expanded {
            return;
        }
        for (message, part) in group {
            self.message = Some(message.id.clone());
            if part.kind == "reasoning" {
                self.thought(message, part, closed);
            } else {
                self.tool(message, part);
            }
        }
    }

    fn thought(&mut self, message: &Message, part: &Part, closed: bool) {
        let text = reasoning_text(part);
        let active = reasoning_active(message, part, closed);
        if text.is_empty() && !active {
            return;
        }
        let (title, body) = reasoning_summary(&text);
        self.gap();
        if active {
            let label = title.map_or("Thinking".to_owned(), |t| format!("Thinking: {t}"));
            self.inline(spinner_frame(), &[(label, None)], self.palette.text);
        } else {
            let label = title.map_or("Thought".to_owned(), |t| format!("Thought: {t}"));
            self.text(INDENT, &label, self.palette.thinking);
        }
        if !body.is_empty() {
            self.gap();
            let start = self.lines.len();
            self.markdown(&message.id, body, INDENT + 2, Some(self.palette.muted));
            for row in &mut self.lines[start..] {
                row.rail = Some((INDENT, self.palette.border));
            }
        }
    }

    /// An icon column followed by wrapped text, like the TUI's InlineToolRow.
    fn inline(
        &mut self,
        icon: &str,
        parts: &[(String, Option<references::Reference>)],
        style: Style,
    ) {
        let mut spans = vec![Span::styled(format!("{icon} "), style)];
        let mut links = vec![None];
        for (text, reference) in parts {
            let span_style = if reference.is_some() {
                style.underline_style(UnderlineStyle::Line)
            } else {
                style
            };
            spans.push(Span::styled(text.clone(), span_style));
            links.push(reference.clone());
        }
        self.push(INDENT, 1, &Spans::from(spans), &links, 2);
    }

    fn block_start(&mut self) -> usize {
        self.gap();
        let start = self.lines.len();
        self.blank();
        start
    }

    fn block_end(&mut self, start: usize) {
        self.blank();
        for row in &mut self.lines[start..] {
            row.fill = Some((INDENT, self.palette.panel));
        }
    }

    fn block_header(&mut self, label: &str, path: Option<(String, Option<references::Reference>)>) {
        let mut spans = vec![Span::styled(label.to_owned(), self.palette.muted)];
        let mut links = vec![None];
        if let Some((path, reference)) = path {
            let style = if reference.is_some() {
                self.palette.muted.underline_style(UnderlineStyle::Line)
            } else {
                self.palette.muted
            };
            spans.push(Span::styled(format!(" {path}"), style));
            links.push(reference);
        }
        self.push(BLOCK_INDENT, 2, &Spans::from(spans), &links, 0);
    }

    fn tool(&mut self, message: &Message, part: &Part) {
        let tool = Tool::new(part);
        let output = strip_ansi(Tool::output(part.state.as_ref()).trim());
        let path = tool
            .path()
            .map(|path| (self.display_path(path), self.reference(message, path)));
        let style = if tool.denied() {
            self.palette.muted.add_modifier(Modifier::CROSSED_OUT)
        } else if tool.failed() {
            self.palette.error
        } else {
            self.palette.muted
        };
        let icon = |default: &'static str| {
            if tool.active() {
                spinner_frame()
            } else {
                default
            }
        };
        let text = |text: String| (text, None);
        match tool.name {
            "read" => {
                self.gap();
                let Some((label, reference)) = path else {
                    self.inline(icon("→"), &[text("Reading file…".into())], style);
                    return self.tool_error(&tool);
                };
                let offset = integer(tool.input, "offset");
                let range = match (offset, integer(tool.input, "limit")) {
                    (start, Some(limit)) if limit > 0 => {
                        let start = start.unwrap_or(1).max(1);
                        format!(":{start}-{}", start + limit - 1)
                    }
                    (Some(start), None) => format!(":{start}"),
                    _ => String::new(),
                };
                self.inline(
                    icon("→"),
                    &[text("Read ".into()), (label, reference), text(range)],
                    style,
                );
                for loaded in tool
                    .metadata
                    .and_then(|m| m.get("loaded"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    let loaded = self.display_path(loaded);
                    self.text(
                        INDENT + 3,
                        &format!("↳ Loaded {loaded}"),
                        self.palette.muted,
                    );
                }
            }
            "glob" | "grep" => {
                self.gap();
                let title = if tool.name == "glob" { "Glob" } else { "Grep" };
                let Some(pattern) = tool.input("pattern") else {
                    let pending = if tool.name == "glob" {
                        "Finding files…"
                    } else {
                        "Searching content…"
                    };
                    self.inline(icon("✱"), &[text(pending.into())], style);
                    return self.tool_error(&tool);
                };
                let mut parts = vec![text(format!("{title} \"{pattern}\""))];
                if let Some((label, reference)) = path.or_else(|| {
                    tool.input("path")
                        .map(|p| (self.display_path(p), self.reference(message, p)))
                }) {
                    parts.push(text(" in ".into()));
                    parts.push((label, reference));
                }
                let key = if tool.name == "glob" {
                    "count"
                } else {
                    "matches"
                };
                if let Some(count) = integer(tool.metadata, key) {
                    let noun = if count == 1 { "match" } else { "matches" };
                    parts.push(text(format!(" ({count} {noun})")));
                }
                self.inline(icon("✱"), &parts, style);
            }
            "webfetch" => {
                self.gap();
                let url = tool.input("url").unwrap_or("…");
                self.inline(icon("%"), &[text(format!("WebFetch {url}"))], style);
            }
            "websearch" => {
                self.gap();
                let query = tool.input("query").unwrap_or("…");
                let label = match string(tool.metadata, "provider") {
                    Some("opencode") => "Web Search via OpenCode".to_owned(),
                    Some(provider) => format!("Web Search via {}", titlecase(provider)),
                    None => "Web Search".to_owned(),
                };
                self.inline(icon("◈"), &[text(format!("{label} \"{query}\""))], style);
            }
            "write" => {
                if tool.status == "completed" {
                    let start = self.block_start();
                    self.block_header("# Wrote", path);
                    if let Some(content) = tool.input("content") {
                        self.blank();
                        self.numbered(content);
                    }
                    self.block_end(start);
                } else {
                    self.gap();
                    match path {
                        Some(path) => self.inline(icon("←"), &[text("Write ".into()), path], style),
                        None => self.inline(icon("←"), &[text("Preparing write…".into())], style),
                    }
                }
            }
            "edit" => {
                let start = self.block_start();
                let file = patch_files(tool.metadata).into_iter().next();
                let path = file
                    .as_ref()
                    .map(|file| (file.path.clone(), self.reference(message, &file.path)))
                    .or(path);
                match &path {
                    Some(_) => self.block_header("← Edit", path),
                    None => self.block_header(&format!("{} Preparing edit…", icon("#")), None),
                }
                if let Some(file) = file {
                    self.blank();
                    self.diff(&file.patch);
                }
                if let Some(error) = &tool.error {
                    self.blank();
                    self.text_with(BLOCK_INDENT, 2, error, self.palette.error);
                }
                self.block_end(start);
            }
            "patch" => {
                let files = patch_files(tool.metadata);
                if files.is_empty() {
                    let start = self.block_start();
                    if tool.failed() {
                        let label = Span::styled("# Patch failed", self.palette.error);
                        self.push(BLOCK_INDENT, 2, &Spans::from(label), &[], 0);
                    } else {
                        self.block_header(&format!("{} Patching…", icon("#")), None);
                    }
                    if let Some(error) = &tool.error {
                        self.blank();
                        self.text_with(BLOCK_INDENT, 2, error, self.palette.error);
                    }
                    self.block_end(start);
                }
                for file in files {
                    let start = self.block_start();
                    let label = match file.kind.as_str() {
                        "add" => "# Created",
                        "delete" => "# Deleted",
                        _ => "← Patched",
                    };
                    let reference = self.reference(message, &file.path);
                    self.block_header(label, Some((file.path.clone(), reference)));
                    self.blank();
                    if file.kind == "delete" {
                        let removed = format!("-{} lines", file.deletions);
                        self.text_with(BLOCK_INDENT, 2, &removed, self.palette.diff_minus);
                    } else {
                        self.diff(&file.patch);
                    }
                    self.block_end(start);
                }
            }
            "shell" => self.shell(
                tool.input("command"),
                tool.input("workdir"),
                &output,
                tool.active(),
                tool.error.as_deref(),
            ),
            "subagent" => {
                self.gap();
                let agent = tool
                    .input("agent")
                    .or_else(|| tool.input("subagent_type"))
                    .map_or("General".to_owned(), titlecase);
                let description = tool.input("description").unwrap_or("");
                let continuation = tool.input("sessionID").is_some();
                let label = if continuation {
                    format!("Continue subagent — {description}")
                } else {
                    format!("{agent} Subagent — {description}")
                };
                let icon = if tool.active() {
                    spinner_frame()
                } else if tool.failed() {
                    "✗"
                } else if continuation {
                    "↳"
                } else {
                    "✓"
                };
                self.inline(icon, &[text(label)], style);
            }
            "todowrite" => {
                let todos: Vec<_> = tool
                    .input
                    .and_then(|input| input.get("todos"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|todo| {
                        Some((
                            string(Some(todo), "content")?,
                            string(Some(todo), "status").unwrap_or("pending"),
                        ))
                    })
                    .collect();
                if tool.status == "completed" && !todos.is_empty() {
                    let start = self.block_start();
                    self.block_header("# Todos", None);
                    self.blank();
                    for (content, status) in todos {
                        let (mark, item_style) = match status {
                            "completed" => ("[✓]", self.palette.text),
                            "in_progress" => ("[•]", self.palette.warning),
                            "cancelled" => (
                                "[-]",
                                self.palette.muted.add_modifier(Modifier::CROSSED_OUT),
                            ),
                            _ => ("[ ]", self.palette.text),
                        };
                        self.push(
                            BLOCK_INDENT,
                            2,
                            &Spans::from(Span::styled(format!("{mark} {content}"), item_style)),
                            &[],
                            4,
                        );
                    }
                    self.block_end(start);
                } else {
                    self.gap();
                    let label = if tool.failed() {
                        "Todo update failed"
                    } else if tool.active() {
                        "Updating todos…"
                    } else {
                        "Cleared todos"
                    };
                    self.inline(icon("⚙"), &[text(label.into())], style);
                }
            }
            "question" => {
                let questions: Vec<_> = tool
                    .input
                    .and_then(|input| input.get("questions"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|question| string(Some(question), "question").unwrap_or(""))
                    .collect();
                let answers = tool
                    .metadata
                    .and_then(|metadata| metadata.get("answers"))
                    .and_then(Value::as_array);
                if let (Some(answers), "completed") = (answers, tool.status) {
                    let start = self.block_start();
                    self.block_header("# Questions", None);
                    for (index, question) in questions.iter().enumerate() {
                        self.blank();
                        self.text_with(BLOCK_INDENT, 2, question, self.palette.muted);
                        let answer = answers
                            .get(index)
                            .and_then(Value::as_array)
                            .map(|answer| {
                                answer
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .filter(|answer| !answer.is_empty())
                            .unwrap_or_else(|| "(no answer)".into());
                        self.text_with(BLOCK_INDENT, 2, &answer, self.palette.text);
                    }
                    self.block_end(start);
                } else {
                    self.gap();
                    let count = questions.len();
                    let label = format!(
                        "Asked {count} question{}",
                        if count == 1 { "" } else { "s" }
                    );
                    self.inline(icon("→"), &[text(label)], style);
                }
            }
            "skill" => {
                self.gap();
                let name = string(tool.metadata, "name")
                    .or_else(|| tool.input("id"))
                    .or_else(|| tool.input("name"))
                    .unwrap_or("skill");
                self.inline(icon("→"), &[text(format!("Skill \"{name}\""))], style);
            }
            "execute" => {
                self.gap();
                let icon = if tool.active() {
                    spinner_frame()
                } else if tool.failed() {
                    "✗"
                } else {
                    "✓"
                };
                self.inline(icon, &[text("execute".into())], style);
                for call in tool
                    .metadata
                    .and_then(|metadata| metadata.get("toolCalls"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let name = string(Some(call), "tool").unwrap_or("tool");
                    let failed = string(Some(call), "status") == Some("error");
                    let summary = primitive_summary(call.get("input"));
                    let label = format!("{} {name}{summary}", if failed { "✗" } else { "›" });
                    let call_style = if failed {
                        self.palette.error
                    } else {
                        self.palette.muted
                    };
                    self.text(INDENT + 2, &label, call_style);
                }
            }
            _ => {
                self.gap();
                let icon = if tool.active() {
                    spinner_frame()
                } else if tool.failed() {
                    "✗"
                } else {
                    "✓"
                };
                let summary = primitive_summary(tool.input);
                let name = part.name.rsplit('.').next().unwrap_or(&part.name);
                self.inline(icon, &[text(format!("{name}{summary}"))], style);
            }
        }
        if !matches!(tool.name, "edit" | "patch" | "shell") {
            self.tool_error(&tool);
        }
    }

    fn tool_error(&mut self, tool: &Tool) {
        if let (Some(error), true) = (&tool.error, tool.failed() && !tool.denied()) {
            self.text(INDENT + 2, error, self.palette.error);
        }
    }

    /// Command and tail-collapsed output in a shaded block.
    fn shell(
        &mut self,
        command: Option<&str>,
        workdir: Option<&str>,
        output: &str,
        active: bool,
        error: Option<&str>,
    ) {
        let start = self.block_start();
        let width = self.content_width(BLOCK_INDENT, 2);
        let Some(command) = command else {
            let label = format!("{} Writing command…", spinner_frame());
            self.text_with(BLOCK_INDENT, 2, &label, self.palette.muted);
            return self.block_end(start);
        };
        let command = match workdir.filter(|dir| *dir != ".") {
            Some(dir) => format!("cd {dir} && {command}"),
            None => command.to_owned(),
        };
        let prefix = if active {
            format!("{} ", spinner_frame())
        } else {
            "$ ".to_owned()
        };
        let mut rows = Vec::new();
        wrap(
            &mut rows,
            &Spans::from(Span::styled(
                format!("{prefix}{command}"),
                self.palette.text,
            )),
            &[],
            width,
            2,
        );
        let mut command_rows = rows.len();
        if command_rows > 2 {
            rows.truncate(2);
            ellipsize(&mut rows[1], width, true);
            command_rows = 2;
        }
        for mut row in rows {
            row.indent = BLOCK_INDENT;
            row.message = self.message.clone();
            self.lines.push(row);
        }
        let lines: Vec<_> = output.lines().collect();
        if !lines.is_empty() {
            let budget = SHELL_LINES.saturating_sub(command_rows + 1).max(1);
            let skip = lines.len().saturating_sub(budget.saturating_sub(1).max(1));
            if lines.len() > budget {
                let label = format!("({skip} earlier lines)");
                self.text_with(BLOCK_INDENT, 2, &label, self.palette.muted);
            } else {
                // The TUI separates command and output with a block gap.
                self.blank();
            }
            let skip = if lines.len() > budget { skip } else { 0 };
            for line in &lines[skip..] {
                let mut row = ChatRow {
                    spans: Spans::from(Span::styled(line.replace('\t', "    "), self.palette.text)),
                    indent: BLOCK_INDENT,
                    message: self.message.clone(),
                    ..ChatRow::default()
                };
                ellipsize(&mut row, width, false);
                self.lines.push(row);
            }
        }
        if let Some(error) = error {
            self.blank();
            self.text_with(BLOCK_INDENT, 2, error, self.palette.error);
        }
        self.block_end(start);
    }

    fn shell_message(&mut self, message: &Message) {
        let output = message
            .output
            .as_ref()
            .and_then(|output| output.get("output"))
            .and_then(Value::as_str)
            .map(|output| strip_ansi(output.trim()))
            .unwrap_or_default();
        let status = message.status.as_deref().unwrap_or("running");
        let error = match (status, message.exit) {
            ("killed", _) => Some("Command cancelled".to_owned()),
            ("timeout", _) => Some("Command timed out".to_owned()),
            ("exited", Some(code)) if code != 0 => Some(format!("Command exited with code {code}")),
            _ => None,
        };
        self.shell(
            message.command.as_deref(),
            None,
            &output,
            status == "running",
            error.as_deref(),
        );
    }

    /// A unified patch with a line-number gutter and colored signs.
    fn diff(&mut self, patch: &str) {
        let mut digits = 1;
        for line in patch.lines().filter(|line| line.starts_with("@@")) {
            for range in line.split_whitespace().skip(1).take(2) {
                let mut numbers = range[1..].split(',');
                let start: u64 = numbers.next().and_then(|n| n.parse().ok()).unwrap_or(0);
                let count: u64 = numbers.next().and_then(|n| n.parse().ok()).unwrap_or(1);
                digits = digits.max((start + count).to_string().len());
            }
        }
        let (mut old, mut new) = (0u64, 0u64);
        let mut started = false;
        let mut shown = 0;
        let mut hidden = 0;
        for line in patch.lines() {
            if let Some(header) = line.strip_prefix("@@") {
                let mut ranges = header.split_whitespace();
                let parse = |range: Option<&str>| {
                    range
                        .and_then(|range| range.get(1..))
                        .and_then(|range| range.split(',').next())
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(1)
                };
                old = parse(ranges.next());
                new = parse(ranges.next());
                if started {
                    if shown < PREVIEW_LINES {
                        let ranges = header.split("@@").next().unwrap_or("").trim();
                        let hunk = Span::styled(format!("@@ {ranges} @@"), self.palette.diff_hunk);
                        self.push(BLOCK_INDENT, 2, &Spans::from(hunk), &[], 0);
                        shown += 1;
                    } else {
                        hidden += 1;
                    }
                }
                started = true;
                continue;
            }
            if !started || line.starts_with('\\') {
                continue;
            }
            let (number, sign, style) = match line.chars().next() {
                Some('+') => {
                    new += 1;
                    (new - 1, "+", self.palette.diff_plus)
                }
                Some('-') => {
                    old += 1;
                    (old - 1, "-", self.palette.diff_minus)
                }
                _ => {
                    old += 1;
                    new += 1;
                    (new - 1, " ", self.palette.text)
                }
            };
            if shown >= PREVIEW_LINES {
                hidden += 1;
                continue;
            }
            shown += 1;
            let content = line.get(1..).unwrap_or("").replace('\t', "    ");
            let spans = Spans::from(vec![
                Span::styled(format!("{number:>digits$} "), self.palette.muted),
                Span::styled(format!("{sign} "), style),
                Span::styled(content, style),
            ]);
            self.push(BLOCK_INDENT, 2, &spans, &[], digits + 3);
        }
        if hidden > 0 {
            let label = format!("… {hidden} more line{}", if hidden == 1 { "" } else { "s" });
            self.text_with(BLOCK_INDENT, 2, &label, self.palette.muted);
        }
    }

    fn numbered(&mut self, content: &str) {
        let lines: Vec<_> = content.lines().collect();
        let digits = lines.len().min(PREVIEW_LINES).to_string().len().max(3);
        for (index, line) in lines.iter().take(PREVIEW_LINES).enumerate() {
            let spans = Spans::from(vec![
                Span::styled(format!("{:>digits$} ", index + 1), self.palette.muted),
                Span::styled(line.replace('\t', "    "), self.palette.text),
            ]);
            self.push(BLOCK_INDENT, 2, &spans, &[], digits + 1);
        }
        if lines.len() > PREVIEW_LINES {
            let hidden = lines.len() - PREVIEW_LINES;
            let label = format!("… {hidden} more line{}", if hidden == 1 { "" } else { "s" });
            self.text_with(BLOCK_INDENT, 2, &label, self.palette.muted);
        }
    }

    /// `Build · provider/model · 12.4s`, shown when a step ends its turn.
    fn footer(&mut self, message: &Message, duration_ms: Option<u64>) {
        let interrupted = message
            .error
            .as_ref()
            .is_some_and(|error| error_text(Some(error)) == "Step interrupted");
        if let Some(error) = message.error.as_ref().filter(|_| !interrupted) {
            self.gap();
            self.text(
                INDENT,
                &format!("Error: {}", error_text(Some(error))),
                self.palette.error,
            );
        }
        self.gap();
        let agent = message
            .agent
            .as_deref()
            .map_or("Assistant".to_owned(), titlecase);
        let agent_style = if message.error.is_some() {
            self.palette.muted
        } else {
            self.palette.agent
        };
        let mut spans = vec![Span::styled(agent, agent_style)];
        if let Some(model) = &message.model {
            spans.push(Span::styled(
                format!(" · {}/{}", model.provider, model.model),
                self.palette.muted,
            ));
        }
        if let Some(ms) = duration_ms.filter(|ms| *ms > 0) {
            spans.push(Span::styled(
                format!(" · {}", duration(ms)),
                self.palette.muted,
            ));
        }
        if interrupted {
            spans.push(Span::styled(" · interrupted", self.palette.muted));
        }
        self.push(INDENT, 1, &Spans::from(spans), &[], 0);
    }

    /// A full-width rule with a centered label.
    fn compaction(&mut self, message: &Message) {
        self.gap();
        let status = message.status.as_deref().unwrap_or("completed");
        let error = message.error.as_ref().map(|error| error_text(Some(error)));
        let cancelled = status == "failed"
            && message
                .error
                .as_ref()
                .and_then(|error| error.get("type"))
                .and_then(Value::as_str)
                == Some("aborted");
        let style = if status == "failed" && !cancelled {
            self.palette.error
        } else {
            self.palette.muted
        };
        let mut label = String::new();
        if status == "running" {
            label.push_str(spinner_frame());
            label.push(' ');
        } else if status == "failed" && !cancelled {
            label.push_str("✗ ");
        }
        label.push_str("Compaction");
        if cancelled {
            label.push_str(" · cancelled");
        }
        if let Some(tokens) = message.tokens.filter(|_| status != "running") {
            let input = tokens.input + tokens.cache.read + tokens.cache.write;
            let output = tokens.output + tokens.reasoning;
            if input + output > 0.0 {
                label.push_str(&format!(" · {} in · {} out", number(input), number(output)));
            }
        }
        let width = self.width.saturating_sub(1);
        let label_width = label.width() + 2;
        let rule = width.saturating_sub(label_width);
        let left = rule / 2;
        let text = format!("{} {label} {}", "─".repeat(left), "─".repeat(rule - left));
        self.text(0, &text, style);
        let content = if status == "failed" {
            error.filter(|_| !cancelled).unwrap_or_default()
        } else {
            message.summary.clone().unwrap_or_default()
        };
        if !content.trim().is_empty() {
            self.blank();
            if status == "failed" {
                self.text(INDENT, content.trim(), self.palette.error);
            } else {
                self.markdown(&message.id, content.trim(), INDENT, None);
            }
        }
    }

    fn switched(&mut self, message: &Message) {
        let previous = message.previous.as_ref();
        let text = if message.kind == "agent-switched" {
            let agent = titlecase(message.agent.as_deref().unwrap_or("agent"));
            match previous.and_then(Value::as_str) {
                Some(previous) if Some(previous) != message.agent.as_deref() => {
                    format!("Switched agent from {} to {agent}", titlecase(previous))
                }
                _ => format!("Switched agent to {agent}"),
            }
        } else {
            let model = message
                .model
                .as_ref()
                .map_or_else(|| "model".to_owned(), |model| model.label());
            format!("Switched model to {model}")
        };
        self.gap();
        self.text(INDENT, &text, self.palette.muted);
    }

    fn moved(&mut self, message: &Message) {
        let Some(location) = &message.location else {
            return;
        };
        self.gap();
        let spans = Spans::from(vec![
            Span::styled("↳ Moved to ", self.palette.muted),
            Span::styled(
                location.directory.to_string_lossy().into_owned(),
                self.palette.info,
            ),
        ]);
        self.push(INDENT, 1, &spans, &[], 0);
    }

    fn notice(&mut self, message: &Message) {
        let metadata = message.metadata.as_ref();
        let source = string(metadata, "source");
        let text = match (message.kind.as_str(), message.description.as_deref()) {
            (_, Some(description)) => description,
            ("system", None) => "Instructions updated",
            _ => return,
        };
        self.gap();
        if matches!(source, Some("subagent" | "shell")) {
            let state = string(metadata, "state");
            let actor = if source == Some("shell") {
                "Shell".to_owned()
            } else {
                titlecase(string(metadata, "agent").unwrap_or("Subagent"))
            };
            let status = match state {
                Some("completed") | None => "finished",
                Some("error") => "failed",
                Some(state) => state,
            };
            let icon = if matches!(state, Some("completed") | None) {
                "↳"
            } else {
                "!"
            };
            let color = match state {
                Some("error") => self.palette.error,
                Some("cancelled") => self.palette.warning,
                _ => self.palette.info,
            };
            let description = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let spans = Spans::from(vec![
                Span::styled(format!("{icon} {actor} {status}"), color),
                Span::styled(format!(" · {description}"), self.palette.muted),
            ]);
            let mut rows = Vec::new();
            wrap(&mut rows, &spans, &[], self.content_width(INDENT, 1), 0);
            if let Some(mut row) = rows.into_iter().next() {
                ellipsize(&mut row, self.content_width(INDENT, 1), false);
                row.indent = INDENT;
                row.message = self.message.clone();
                self.lines.push(row);
            }
        } else {
            let icon = if source == Some("goal") { "◎" } else { "◈" };
            self.inline(icon, &[(text.to_owned(), None)], self.palette.muted);
        }
    }
}

struct PatchFile {
    kind: String,
    path: String,
    patch: String,
    deletions: u64,
}

fn patch_files(metadata: Option<&Value>) -> Vec<PatchFile> {
    metadata
        .and_then(|metadata| metadata.get("files"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|file| {
            let status = string(Some(file), "status");
            let kind = string(Some(file), "type").or(match status {
                Some("added") => Some("add"),
                Some("deleted") => Some("delete"),
                Some("modified") => Some("update"),
                _ => None,
            })?;
            Some(PatchFile {
                kind: kind.to_owned(),
                path: string(Some(file), "relativePath")
                    .or_else(|| string(Some(file), "file"))?
                    .to_owned(),
                patch: string(Some(file), "patch").unwrap_or("").to_owned(),
                deletions: integer(Some(file), "deletions").unwrap_or(0),
            })
        })
        .collect()
}

fn primitive_summary(input: Option<&Value>) -> String {
    let entries: Vec<_> = input
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| {
            let value = match value {
                Value::String(text) => text.clone(),
                Value::Number(number) => number.to_string(),
                Value::Bool(flag) => flag.to_string(),
                _ => return None,
            };
            Some(format!(
                "{key}={}",
                value.split_whitespace().collect::<Vec<_>>().join(" ")
            ))
        })
        .collect();
    if entries.is_empty() {
        String::new()
    } else {
        format!(" [{}]", entries.join(", "))
    }
}

pub(super) fn selection_comments(metadata: Option<&Value>) -> Vec<String> {
    metadata
        .and_then(|m| m.get("comments"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let comment = string(Some(item), "comment").filter(|text| !text.trim().is_empty())?;
            let path = string(Some(item), "path").unwrap_or("Quoted selection");
            let line = item
                .get("selection")
                .and_then(|s| s.get("startLine"))
                .and_then(Value::as_u64);
            let label = line.map_or_else(|| path.to_owned(), |line| format!("{path}:{line}"));
            Some(format!("{label}: {comment}"))
        })
        .collect()
}

/// Replace the end of a row with `…` when it does not fit in `width` columns,
/// or unconditionally when `force` marks omitted content.
fn ellipsize(row: &mut ChatRow, width: usize, force: bool) {
    if !force && row.spans.width() <= width {
        return;
    }
    let mut used = 0;
    let budget = width.saturating_sub(1);
    let mut spans = Vec::new();
    for span in row.spans.0.drain(..) {
        let mut text = String::new();
        for grapheme in span.content.graphemes(true) {
            if used + grapheme.width() > budget {
                break;
            }
            used += grapheme.width();
            text.push_str(grapheme);
        }
        let full = text.len() == span.content.len();
        spans.push(Span::styled(text, span.style));
        if !full {
            break;
        }
    }
    if let Some(last) = spans.last_mut() {
        last.content.to_mut().push('…');
    }
    row.spans = Spans::from(spans);
}

/// Wrap at word boundaries, preserving inline styles and link spans. Bullet
/// and numbered-list continuations align with their text; `hang` indents
/// continuations of icon rows. Control sequences and over-wide glyphs are
/// never passed to the compositor.
pub(super) fn wrap(
    lines: &mut Vec<ChatRow>,
    spans: &Spans<'_>,
    links: &[Option<references::Reference>],
    width: usize,
    hang: usize,
) {
    if width == 0 {
        return;
    }
    let bullet = spans.0.first().map_or(0, |span| {
        let marker = span.content.trim();
        if marker == "•"
            || marker.strip_suffix('.').is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|c| c.is_ascii_digit())
            })
        {
            span.content.width()
        } else {
            0
        }
    });
    let indent = hang.max(bullet).min(width / 2);
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_links = Vec::new();
    let mut used = 0;
    for (span_index, span) in spans.0.iter().enumerate() {
        let reference = links.get(span_index).and_then(Option::as_ref);
        for word in span.content.split_word_bounds() {
            if used > indent && word.width() <= width - indent && used + word.width() > width {
                lines.push(ChatRow {
                    spans: Spans::from(std::mem::take(&mut current)),
                    links: std::mem::take(&mut current_links),
                    ..ChatRow::default()
                });
                used = indent;
                if indent > 0 {
                    current.push(Span::raw(" ".repeat(indent)));
                }
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
                    used = indent;
                    if indent > 0 {
                        current.push(Span::raw(" ".repeat(indent)));
                    }
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
