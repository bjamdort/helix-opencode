//! Local file references are data, never shell commands or browser launches.
use super::*;
use crate::opencode::Message;
use anyhow::{Context as _, Result};
use pulldown_cmark::{Event as MarkdownEvent, Parser, Tag, TagEnd};
use std::{
    collections::HashSet,
    path::{Component as PathComponent, Path},
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileTarget {
    pub path: PathBuf,
    /// One-based source coordinates, as written in the conversation.
    pub line: Option<usize>,
    pub column: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Reference {
    pub message_id: String,
    pub directory: PathBuf,
    pub target: FileTarget,
}

impl FileTarget {
    pub fn label(&self) -> String {
        let mut label = self.path.to_string_lossy().into_owned();
        if let Some(line) = self.line {
            label.push_str(&format!(":{line}"));
        }
        if let Some(column) = self.column {
            label.push_str(&format!(":{column}"));
        }
        label
    }

    pub fn resolve(&self, directory: &Path) -> Result<PathBuf> {
        let root = directory
            .canonicalize()
            .context("Session directory is not available locally")?;
        anyhow::ensure!(
            !self
                .path
                .components()
                .any(|part| matches!(part, PathComponent::ParentDir)),
            "References cannot traverse outside the workspace"
        );
        let path = root
            .join(&self.path)
            .canonicalize()
            .context("Referenced file does not exist locally")?;
        anyhow::ensure!(
            path.starts_with(&root),
            "Reference is outside the session workspace"
        );
        anyhow::ensure!(path.is_file(), "Reference is not a file");
        Ok(path)
    }

    pub fn position(&self) -> Option<Position> {
        self.line.map(|line| {
            Position::new(
                line.saturating_sub(1),
                self.column.unwrap_or(1).saturating_sub(1),
            )
        })
    }
}

impl Reference {
    pub fn outcome(&self) -> Result<PaneOutcome> {
        Ok(PaneOutcome::OpenFileAt {
            path: self.target.resolve(&self.directory)?,
            position: self.target.position(),
        })
    }
}

fn positive(text: &str) -> Option<usize> {
    let value: usize = text.parse().ok()?;
    (value > 0).then_some(value)
}

fn decode_path(path: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut input = path.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let high = (input.next()? as char).to_digit(16)?;
            let low = (input.next()? as char).to_digit(16)?;
            bytes.push((high * 16 + low) as u8);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok()
}

/// Accept common editor/compiler/GitHub reference formats. Unknown URI schemes,
/// web links, heading anchors, and malformed coordinates are not file targets.
pub(super) fn parse(input: &str, explicit: bool) -> Option<FileTarget> {
    let input = input.trim();
    if input.is_empty() || input.chars().any(char::is_control) {
        return None;
    }
    let (path, fragment) = input
        .split_once('#')
        .map_or((input, None), |(path, fragment)| (path, Some(fragment)));
    let (mut path, mut line, mut column) = (path.to_owned(), None, None);
    if let Some(fragment) = fragment {
        let start = fragment.split('-').next()?.strip_prefix('L')?;
        if let Some((row, col)) = start.split_once('C') {
            line = Some(positive(row)?);
            column = Some(positive(col)?);
        } else {
            line = Some(positive(start)?);
        }
    } else if let Some((prefix, suffix)) = path.rsplit_once(':') {
        if let Some(last) = positive(suffix) {
            if let Some((file, row)) = prefix.rsplit_once(':') {
                // file:/// has a scheme colon, which is not a coordinate.
                if let Some(row) = positive(row) {
                    path = file.to_owned();
                    line = Some(row);
                    column = Some(last);
                } else {
                    path = prefix.to_owned();
                    line = Some(last);
                }
            } else {
                path = prefix.to_owned();
                line = Some(last);
            }
        } else if !path.starts_with("file://") {
            return None;
        }
    }
    let path = if path.starts_with("file://") {
        let url = reqwest::Url::parse(&path).ok()?;
        if url.query().is_some()
            || url
                .host_str()
                .is_some_and(|host| !host.is_empty() && host != "localhost")
        {
            return None;
        }
        url.to_file_path().ok()?.to_string_lossy().into_owned()
    } else {
        if path.contains(':') || path.contains('?') {
            return None;
        }
        decode_path(&path)?
    };
    if path.is_empty() || path.chars().any(char::is_control) || path.contains('\\') {
        return None;
    }
    let target = Path::new(&path);
    let name = target.file_name()?.to_str()?;
    let looks_like_path = path.contains('/')
        || name.starts_with('.')
        || name.contains('.') && name.chars().any(char::is_alphabetic)
        || matches!(
            name,
            "Dockerfile" | "Makefile" | "Justfile" | "LICENSE" | "README" | "AGENTS.md"
        );
    if !explicit && !looks_like_path {
        return None;
    }
    if name == "." || name == ".." {
        return None;
    }
    Some(FileTarget {
        path: target.to_owned(),
        line,
        column,
    })
}

/// Byte ranges in already-rendered prose; never reinterpret a web link's label.
pub(super) fn inline_targets(
    text: &str,
    destination: Option<&str>,
    code: bool,
) -> Vec<(std::ops::Range<usize>, FileTarget)> {
    if let Some(destination) = destination {
        return parse(destination, true)
            .map(|target| vec![(0..text.len(), target)])
            .unwrap_or_default();
    }
    if code {
        return parse(text, false)
            .map(|target| vec![(0..text.len(), target)])
            .unwrap_or_default();
    }
    let mut result = Vec::new();
    let mut offset = 0;
    for word in text.split_inclusive(char::is_whitespace) {
        let trimmed = word
            .trim()
            .trim_start_matches(['(', '[', '<', '"', '\''])
            .trim_end_matches([')', ']', '>', '"', '\'', ',', ';', '.', ':']);
        if let Some(target) = parse(trimmed, false) {
            let start = offset + word.find(trimmed).unwrap_or(0);
            result.push((start..start + trimmed.len(), target));
        }
        offset += word.len();
    }
    result
}

pub(super) fn from_attachment(file: &serde_json::Value) -> Option<FileTarget> {
    let source = file.get("source")?;
    if source.get("type")?.as_str()? != "uri" {
        return None;
    }
    let mut url = reqwest::Url::parse(source.get("uri")?.as_str()?).ok()?;
    let line = url
        .query_pairs()
        .find(|(key, _)| key == "start")
        .and_then(|(_, value)| positive(&value));
    url.set_query(None);
    let mut target = parse(url.as_str(), true)?;
    target.line = line;
    Some(target)
}

fn text_targets(text: &str, targets: &mut Vec<FileTarget>) {
    let mut link_depth = 0usize;
    let mut code_block = false;
    for event in Parser::new(text) {
        match event {
            MarkdownEvent::Start(Tag::Link { dest_url, .. }) => {
                link_depth += 1;
                if let Some(target) = parse(&dest_url, true) {
                    targets.push(target);
                }
            }
            MarkdownEvent::Start(Tag::Image { .. }) => link_depth += 1,
            MarkdownEvent::End(TagEnd::Link | TagEnd::Image) => {
                link_depth = link_depth.saturating_sub(1)
            }
            MarkdownEvent::Start(Tag::CodeBlock(_)) => code_block = true,
            MarkdownEvent::End(TagEnd::CodeBlock) => code_block = false,
            MarkdownEvent::Code(code) if link_depth == 0 => {
                if let Some(target) = parse(&code, false) {
                    targets.push(target);
                }
            }
            MarkdownEvent::Text(text) if link_depth == 0 && !code_block => {
                for word in text.split_whitespace() {
                    let word = word
                        .trim_start_matches(['(', '[', '<', '"', '\''])
                        .trim_end_matches([')', ']', '>', '"', '\'', ',', ';', '.', ':']);
                    if let Some(target) = parse(word, false) {
                        targets.push(target);
                    }
                }
            }
            _ => (),
        }
    }
}

pub(super) fn for_message(message: &Message, directory: &Path) -> Vec<Reference> {
    let mut targets = Vec::new();
    if message.kind == "user" {
        text_targets(&message.text, &mut targets);
        targets.extend(message.files.iter().filter_map(from_attachment));
    }
    if message.kind == "assistant" {
        for part in &message.content {
            if part.kind == "text" {
                text_targets(&part.text, &mut targets);
            }
            if part.kind == "tool" {
                if let Some(input) = part.state.as_ref().and_then(|tool| tool.input.as_ref()) {
                    for key in ["path", "file_path", "filePath", "filename"] {
                        if let Some(path) = input.get(key).and_then(serde_json::Value::as_str) {
                            if let Some(mut target) = parse(path, true) {
                                target.line = target.line.or_else(|| {
                                    input
                                        .get("offset")
                                        .and_then(serde_json::Value::as_u64)
                                        .and_then(|v| usize::try_from(v).ok())
                                        .filter(|v| *v > 0)
                                });
                                targets.push(target);
                            }
                        }
                    }
                }
                if let Some(tool) = &part.state {
                    for content in &tool.content {
                        if content.get("type").and_then(serde_json::Value::as_str) == Some("file") {
                            if let Some(uri) =
                                content.get("uri").and_then(serde_json::Value::as_str)
                            {
                                if let Some(target) = parse(uri, true) {
                                    targets.push(target);
                                }
                            }
                        }
                        if matches!(
                            part.name.rsplit('.').next(),
                            Some("glob" | "grep" | "rg" | "find")
                        ) {
                            if let Some(text) =
                                content.get("text").and_then(serde_json::Value::as_str)
                            {
                                // Search tools can return huge results. Match the bounded
                                // transcript philosophy rather than parsing unbounded output.
                                let text: String = text.chars().take(65_536).collect();
                                text_targets(&text, &mut targets);
                                for line in text.lines() {
                                    let mut pieces = line.split(':');
                                    if let (Some(path), Some(row)) = (pieces.next(), pieces.next())
                                    {
                                        if let Some(row) = positive(row) {
                                            let col = pieces.next().and_then(positive);
                                            let location = col.map_or_else(
                                                || format!("{path}:{row}"),
                                                |col| format!("{path}:{row}:{col}"),
                                            );
                                            if let Some(target) = parse(&location, false) {
                                                targets.push(target);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let mut seen = HashSet::new();
    targets
        .into_iter()
        .filter(|target| seen.insert(target.clone()))
        .map(|target| Reference {
            message_id: message.id.clone(),
            directory: directory.to_owned(),
            target,
        })
        .collect()
}

/// Opening an existing file never reloads or writes its buffer. Coordinates are
/// clamped by Helix's grapheme-aware position conversion.
pub fn open_file(editor: &mut Editor, path: &Path, position: Option<Position>) -> Result<()> {
    editor.open(path, helix_view::editor::Action::Replace)?;
    if let Some(position) = position {
        let scrolloff = editor.config().scrolloff;
        let (view, doc) = helix_view::current!(editor);
        let pos = helix_core::pos_at_coords(doc.text().slice(..), position, true);
        doc.set_selection(view.id, helix_core::Selection::point(pos));
        view.ensure_cursor_in_view_center(doc, scrolloff);
    }
    Ok(())
}

pub(super) struct ReferenceList {
    references: Vec<Reference>,
    panel: Arc<Mutex<Rect>>,
    selected: usize,
    error: Option<String>,
}

impl ReferenceList {
    pub fn new(references: Vec<Reference>, panel: Arc<Mutex<Rect>>) -> Self {
        Self {
            references,
            panel,
            selected: 0,
            error: None,
        }
    }
}

impl crate::compositor::Component for ReferenceList {
    fn handle_event(
        &mut self,
        event: &crate::compositor::Event,
        _cx: &mut crate::compositor::Context,
    ) -> crate::compositor::EventResult {
        use crate::{
            compositor::{Event, EventResult},
            key,
        };
        let Event::Key(key) = event else {
            return EventResult::Consumed(None);
        };
        match *key {
            key!(Esc) | key!('q') => {
                return EventResult::Consumed(Some(Box::new(|compositor, _| {
                    compositor.pop();
                })))
            }
            key!('j') | key!(Down) => {
                self.selected = (self.selected + 1).min(self.references.len().saturating_sub(1))
            }
            key!('k') | key!(Up) => self.selected = self.selected.saturating_sub(1),
            key!(Enter) | key!('o') => {
                if let Some(reference) = self.references.get(self.selected) {
                    match reference.target.resolve(&reference.directory) {
                        Ok(path) => {
                            let position = reference.target.position();
                            return EventResult::Consumed(Some(Box::new(move |compositor, cx| {
                                match open_file(cx.editor, &path, position) {
                                    Ok(()) => {
                                        compositor.pop();
                                        if let Some(view) =
                                            compositor.find::<crate::ui::EditorView>()
                                        {
                                            view.panes.focus_document(cx.editor);
                                        }
                                    }
                                    Err(error) => cx.editor.set_error(error.to_string()),
                                }
                            })));
                        }
                        Err(error) => self.error = Some(format!("{error:#}")),
                    }
                }
            }
            _ => (),
        }
        EventResult::Consumed(None)
    }

    fn render(
        &mut self,
        viewport: Rect,
        surface: &mut Buffer,
        cx: &mut crate::compositor::Context,
    ) {
        let area = self.panel.lock().intersection(viewport);
        if area.width < 8 || area.height < 5 {
            return;
        }
        fill(area, surface, cx.editor.theme.get("ui.popup"));
        let inner = area.clip_left(2).clip_right(1).clip_top(1).clip_bottom(1);
        write(
            inner.with_height(1),
            surface,
            "File references",
            cx.editor.theme.get("ui.text.info"),
        );
        let body = inner.clip_top(2).clip_bottom(2);
        let start = self
            .selected
            .saturating_sub(body.height.saturating_sub(1) as usize);
        for (row, (index, reference)) in self
            .references
            .iter()
            .enumerate()
            .skip(start)
            .take(body.height as usize)
            .enumerate()
        {
            write(
                Rect::new(body.x, body.y + row as u16, body.width, 1),
                surface,
                &format!(
                    "{} {}. {}",
                    if index == self.selected { "›" } else { " " },
                    index + 1,
                    reference.target.label()
                ),
                cx.editor.theme.get(if index == self.selected {
                    "ui.text.focus"
                } else {
                    "ui.text"
                }),
            );
        }
        if self.references.is_empty() {
            write(
                body,
                surface,
                "No file references in the loaded history.",
                cx.editor.theme.get("ui.text.inactive"),
            );
        }
        if let Some(error) = &self.error {
            write(
                Rect::new(inner.x, inner.bottom() - 2, inner.width, 1),
                surface,
                error,
                cx.editor.theme.get("error"),
            );
        }
        write(
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            surface,
            "j/k select   Enter open   Esc close",
            cx.editor.theme.get("ui.text.inactive"),
        );
    }

    fn cursor(&self, _area: Rect, _editor: &Editor) -> (Option<Position>, CursorKind) {
        (Some(Position::default()), CursorKind::Hidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_formats_preserve_one_based_coordinates_and_spaces() {
        for (text, path, line, column) in [
            ("src/main.rs:12", "src/main.rs", Some(12), None),
            ("src/main.rs:12:4", "src/main.rs", Some(12), Some(4)),
            ("src/main.rs#L12", "src/main.rs", Some(12), None),
            ("src/main.rs#L12C4-L20", "src/main.rs", Some(12), Some(4)),
            ("src/a%20file.rs#L2-L5", "src/a file.rs", Some(2), None),
            ("src/a file.rs:2", "src/a file.rs", Some(2), None),
            ("file:///work/main.rs", "/work/main.rs", None, None),
            (
                "file:///work/main.rs:2:3",
                "/work/main.rs",
                Some(2),
                Some(3),
            ),
            ("Cargo.toml", "Cargo.toml", None, None),
        ] {
            let target = parse(text, false).unwrap_or_else(|| panic!("failed to parse {text}"));
            assert_eq!(target.path, Path::new(path));
            assert_eq!(target.line, line);
            assert_eq!(target.column, column);
        }
        for text in [
            "https://example.com/main.rs",
            "javascript:alert(1)",
            "file://remote/work/a.rs",
            "src/main.rs:0",
            "src/main.rs:0:2",
            "src/main.rs:9999999999999999999999999999",
            "std::io::Result",
            "src/main.rs#section",
            "src/%00file.rs",
            "src/%ZZ.rs",
            "src/main.rs\nbad",
        ] {
            assert!(
                parse(text, true).is_none(),
                "unexpected file target: {text}"
            );
        }
    }

    #[test]
    fn markdown_links_inline_and_plain_paths_are_collected_without_web_labels() {
        let mut targets = Vec::new();
        text_targets("Read [the implementation](src/main.rs#L12), `src/a file.rs:3`, and src/lib.rs:8:2.\n\
            [src/not-local.rs](https://example.com/page) and `std::io::Result` are not local links.\n\
            ![src/also-not-local.rs](https://example.com/image.png)\n\
            ```rust\nlet filename = \"src/not-a-reference.rs\";\n```", &mut targets);
        let paths: Vec<_> = targets.iter().map(FileTarget::label).collect();
        assert_eq!(
            paths,
            ["src/main.rs:12", "src/a file.rs:3", "src/lib.rs:8:2"]
        );
    }

    #[test]
    fn messages_deduplicate_references_and_include_tool_input_positions() {
        let message: Message = serde_json::from_value(serde_json::json!({
            "id":"msg_test", "type":"assistant", "content":[
                {"type":"text","text":"See `src/main.rs:4` and [again](src/main.rs#L4)."},
                {"type":"tool","name":"read","state":{"status":"completed","input":{"path":"src/lib.rs","offset":20}}}
            ]
        })).unwrap();
        let references = for_message(&message, Path::new("/work"));
        assert_eq!(references.len(), 2);
        assert_eq!(references[0].target.label(), "src/main.rs:4");
        assert_eq!(references[1].target.label(), "src/lib.rs:20");
        assert_eq!(references[1].message_id, "msg_test");
    }

    #[test]
    fn search_results_and_file_content_expose_existing_reference_formats() {
        let message: Message = serde_json::from_value(serde_json::json!({
            "id":"msg_search", "type":"assistant", "content":[
                {"type":"tool","name":"grep","state":{"status":"completed","content":[
                    {"type":"text","text":"src/main.rs:24:3:matched text\nsrc/other.rs:8:matched text"}
                ]}},
                {"type":"tool","name":"generate","state":{"status":"completed","content":[
                    {"type":"file","uri":"file:///work/result.txt"}
                ]}}
            ]
        })).unwrap();
        let references = for_message(&message, Path::new("/work"));
        let labels: Vec<_> = references
            .iter()
            .map(|reference| reference.target.label())
            .collect();
        assert_eq!(
            labels,
            ["src/main.rs:24:3", "src/other.rs:8", "/work/result.txt"]
        );
    }

    #[test]
    fn opening_never_creates_files_or_escapes_the_workspace() -> Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("source.rs"), "hello")?;
        let target = parse("source.rs:50:3", false).unwrap();
        assert_eq!(target.resolve(root.path())?, root.path().join("source.rs"));
        let absolute = FileTarget {
            path: root.path().join("source.rs"),
            ..target.clone()
        };
        assert!(absolute.resolve(root.path()).is_ok());
        for path in ["missing.rs", "../outside.rs", "/etc/passwd", "."] {
            let target = FileTarget {
                path: path.into(),
                line: None,
                column: None,
            };
            assert!(target.resolve(root.path()).is_err());
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", root.path().join("external.rs"))?;
            let target = parse("external.rs", false).unwrap();
            assert!(target.resolve(root.path()).is_err());
        }
        assert!(!root.path().join("missing.rs").exists());
        Ok(())
    }

    #[test]
    fn coordinates_are_grapheme_aware_and_clamp_to_existing_text() {
        let text = helix_core::Rope::from("first\né界x\n");
        let target = parse("source.rs:2:3", false).unwrap();
        let position = helix_core::pos_at_coords(text.slice(..), target.position().unwrap(), true);
        assert_eq!(text.char(position), 'x');
        let target = parse("source.rs:999:999", false).unwrap();
        let position = helix_core::pos_at_coords(text.slice(..), target.position().unwrap(), true);
        assert_eq!(position, 9);
        let empty = helix_core::Rope::from("");
        assert_eq!(
            helix_core::pos_at_coords(empty.slice(..), target.position().unwrap(), true),
            0
        );
    }
}
