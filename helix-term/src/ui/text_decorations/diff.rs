//! Shades changed lines and draws removed lines below their anchor line, in
//! the virtual lines that `helix_view::annotations::diff` reserves.
use std::{cell::RefCell, collections::HashMap, rc::Rc};

use helix_core::{
    doc_formatter::FormattedGrapheme,
    line_ending::line_end_char_index,
    syntax,
    unicode::{segmentation::UnicodeSegmentation, width::UnicodeWidthStr},
    Position, RopeSlice,
};
use helix_view::{
    annotations::diff::InlineDiff,
    graphics::{Color, Rect, Style},
    Theme,
};

use crate::ui::{
    document::{LinePos, TextRenderer},
    text_decorations::Decoration,
};

/// A dark background for added or removed lines. Themes can set one with
/// `diff.plus.background` and `diff.minus.background`; otherwise it is a dark
/// shade of the theme's `diff.plus` or `diff.minus` color.
pub fn tint(theme: &Theme, scope: &str, fallback: Color) -> Color {
    if let Some(style) = theme.try_get_exact(&format!("{scope}.background")) {
        if let Some(color) = style.bg.or(style.fg) {
            return color;
        }
    }
    match theme.get(scope).fg {
        Some(Color::Rgb(r, g, b)) => {
            let shade = |c: u8| (c as u16 * 28 / 100) as u8;
            Color::Rgb(shade(r), shade(g), shade(b))
        }
        _ => fallback,
    }
}

pub fn added_background(theme: &Theme) -> Color {
    tint(theme, "diff.plus", Color::Rgb(0x1c, 0x3b, 0x27))
}

pub fn removed_background(theme: &Theme) -> Color {
    tint(theme, "diff.minus", Color::Rgb(0x4a, 0x1d, 0x22))
}

type Line = Vec<(String, Style)>;

/// Highlighted removed blocks, reused across frames. Keyed by language and text.
#[derive(Default)]
pub struct HighlightCache {
    blocks: HashMap<(String, String), Rc<Vec<Line>>>,
}

impl HighlightCache {
    const CAPACITY: usize = 256;

    fn get(
        &mut self,
        text: String,
        language: Option<&str>,
        theme: &Theme,
        loader: &syntax::Loader,
    ) -> Rc<Vec<Line>> {
        let key = (language.unwrap_or_default().to_owned(), text);
        if let Some(lines) = self.blocks.get(&key) {
            return lines.clone();
        }
        if self.blocks.len() >= Self::CAPACITY {
            self.blocks.clear();
        }
        let lines: Vec<Line> = match language {
            Some(language) => crate::ui::markdown::highlighted_code_block(
                &key.1,
                language,
                Some(theme),
                loader,
                None,
            )
            .lines
            .into_iter()
            .map(|line| {
                line.0
                    .into_iter()
                    .map(|span| (span.content.into_owned(), span.style))
                    .collect()
            })
            .collect(),
            None => key
                .1
                .lines()
                .map(|line| vec![(line.to_owned(), theme.get("ui.text"))])
                .collect(),
        };
        let lines = Rc::new(lines);
        self.blocks.insert(key, lines.clone());
        lines
    }
}

pub struct InlineDiffDecoration<'a> {
    text: RopeSlice<'a>,
    diff: InlineDiff,
    /// The view area, so backgrounds also cover the gutter.
    area: Rect,
    added: Style,
    removed: Style,
    tab_width: usize,
    language: Option<String>,
    theme: &'a Theme,
    loader: &'a syntax::Loader,
    cache: &'a RefCell<HighlightCache>,
    /// The next removed block whose anchor line break has not been reached.
    next: usize,
    /// A removed block whose anchor line break was just rendered.
    armed: Option<usize>,
}

impl<'a> InlineDiffDecoration<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        text: RopeSlice<'a>,
        diff: InlineDiff,
        area: Rect,
        tab_width: usize,
        language: Option<String>,
        theme: &'a Theme,
        loader: &'a syntax::Loader,
        cache: &'a RefCell<HighlightCache>,
    ) -> Self {
        Self {
            text,
            diff,
            area,
            added: Style::default().bg(added_background(theme)),
            removed: Style::default().bg(removed_background(theme)),
            tab_width,
            language,
            theme,
            loader,
            cache,
            next: 0,
            armed: None,
        }
    }

    fn anchor(&self, index: usize) -> usize {
        self.diff.removed.get(index).map_or(usize::MAX, |block| {
            line_end_char_index(&self.text, block.anchor.min(self.text.len_lines() - 1))
        })
    }

    fn removed_lines(&self, index: usize) -> Vec<Line> {
        let block = &self.diff.removed[index];
        let base = &self.diff.base;
        let mut lines = Vec::with_capacity(block.len());
        // Highlight each contiguous range on its own so it parses as a unit.
        for range in &block.ranges {
            let end = range.end.min(base.len_lines());
            let start = range.start.min(end);
            let text: String = base
                .slice(base.line_to_char(start)..base.line_to_char(end))
                .into();
            let highlighted = self.cache.borrow_mut().get(
                text,
                self.language.as_deref(),
                self.theme,
                self.loader,
            );
            // Exactly one row per base line: highlighting omits a trailing
            // empty line, and the reserved rows must match the drawn rows.
            let expected = lines.len() + (end - start);
            lines.extend(highlighted.iter().take(end - start).cloned());
            lines.resize_with(expected, Vec::new);
        }
        lines
    }
}

impl Decoration for InlineDiffDecoration<'_> {
    fn decorate_line(&mut self, renderer: &mut TextRenderer, pos: LinePos) {
        if self.diff.is_added(pos.doc_line) {
            let area = Rect::new(self.area.x, pos.visual_line, self.area.width, 1);
            renderer.set_style(area, self.added);
        }
    }

    fn reset_pos(&mut self, pos: usize) -> usize {
        self.armed = None;
        self.next = 0;
        while self.next < self.diff.removed.len() && self.anchor(self.next) < pos {
            self.next += 1;
        }
        self.anchor(self.next)
    }

    fn decorate_grapheme(
        &mut self,
        _renderer: &mut TextRenderer,
        grapheme: &FormattedGrapheme,
    ) -> usize {
        if self.anchor(self.next) == grapheme.char_idx {
            self.armed = Some(self.next);
            self.next += 1;
        }
        self.anchor(self.next)
    }

    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        let Some(index) = self
            .armed
            .filter(|index| self.diff.removed[*index].anchor == pos.doc_line)
        else {
            return Position::new(0, 0);
        };
        self.armed = None;
        let lines = self.removed_lines(index);
        let count = self.diff.removed[index].len();
        let bottom = renderer.offset.row + renderer.viewport.height as usize;
        for (row, line) in lines.iter().take(count).enumerate() {
            let y = pos.visual_line as usize + virt_off.row + row;
            if y < renderer.offset.row {
                continue;
            }
            if y >= bottom {
                break;
            }
            let y = y as u16;
            renderer.set_style(Rect::new(self.area.x, y, self.area.width, 1), self.removed);
            let mut column = 0;
            let first = renderer.offset.col;
            let last = first + renderer.viewport.width as usize;
            'line: for (text, style) in line {
                for grapheme in text.graphemes(true) {
                    let tab_width = self.tab_width.max(1);
                    let tab = " ".repeat(tab_width - column % tab_width);
                    let (grapheme, width) = match grapheme {
                        "\t" => (tab.as_str(), tab.len()),
                        g if g.chars().any(char::is_control) => continue,
                        g => (g, g.width()),
                    };
                    if column + width > last {
                        break 'line;
                    }
                    if column >= first {
                        let x = renderer.viewport.x + (column - first) as u16;
                        renderer.set_string(x, y, grapheme, style.patch(self.removed));
                    }
                    column += width;
                }
            }
        }
        Position::new(count, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tints_are_dark_shades_of_the_theme_diff_colors() {
        let theme = Theme::from(
            toml::from_str::<toml::Value>(
                r##"
            "diff.plus" = "#40c080"
            "diff.minus.background" = { bg = "#300000" }
        "##,
            )
            .unwrap(),
        );
        assert_eq!(
            tint(&theme, "diff.plus", Color::Reset),
            Color::Rgb(17, 53, 35)
        );
        assert_eq!(
            tint(&theme, "diff.minus", Color::Reset),
            Color::Rgb(0x30, 0, 0)
        );
        assert_eq!(tint(&theme, "diff.delta", Color::Red), Color::Red);
    }

    async fn document(base: &str, text: &str) -> (helix_view::Document, helix_view::View) {
        use arc_swap::ArcSwap;
        use std::sync::Arc;
        let mut view = helix_view::View::new(
            helix_view::DocumentId::default(),
            helix_view::editor::GutterConfig::default(),
        );
        view.area = Rect::new(0, 0, 30, 8);
        let mut doc = helix_view::Document::from(
            helix_core::Rope::from(text),
            None,
            Arc::new(ArcSwap::new(
                Arc::new(helix_view::editor::Config::default()),
            )),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );
        doc.ensure_view_init(view.id);
        doc.set_custom_diff_base(base.as_bytes().to_vec(), "test base".into());
        doc.inline_diff = true;
        for _ in 0..200 {
            if !doc.diff_handle().unwrap().load().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        (doc, view)
    }

    #[tokio::test]
    async fn removed_lines_render_above_their_replacement_on_tinted_rows() {
        let theme = &helix_view::theme::DEFAULT_THEME;
        let (doc, view) = document("keep\nold one\nold two\nend\n", "keep\nnew\nend\n").await;
        assert_eq!(doc.diff_source(), Some("test base"));
        let area = Rect::new(0, 0, 30, 8);
        let annotations = view.text_annotations(&doc, None);
        let diff = InlineDiff::for_document(&doc).unwrap();
        let cache = RefCell::default();
        let loader = syntax::Loader::default();
        let mut decorations = crate::ui::text_decorations::DecorationManager::default();
        decorations.add_decoration(InlineDiffDecoration::new(
            doc.text().slice(..),
            diff,
            area,
            4,
            None,
            theme,
            &loader,
            &cache,
        ));
        let mut surface = tui::buffer::Buffer::empty(area);
        crate::ui::document::render_document(
            &mut surface,
            area,
            &doc,
            doc.view_offset(view.id),
            &annotations,
            None,
            Vec::new(),
            theme,
            decorations,
        );
        let row = |y: u16| {
            (0..area.width)
                .map(|x| surface.get(x, y).unwrap().symbol.to_string())
                .collect::<String>()
                .trim_end()
                .to_owned()
        };
        let rows: Vec<_> = (0..5).map(row).collect();
        assert_eq!(rows, ["keep", "old one", "old two", "new", "end"]);
        let background = |y: u16| surface.get(area.width - 1, y).unwrap().bg;
        assert_eq!(background(1), removed_background(theme));
        assert_eq!(background(2), removed_background(theme));
        assert_eq!(background(3), added_background(theme));
        assert_ne!(background(0), added_background(theme));
        assert_ne!(background(4), removed_background(theme));
    }

    #[tokio::test]
    async fn pure_removals_add_no_shading_and_hiding_removes_them() {
        let (doc, _) = document("a\nremoved\nb\n", "a\nb\n").await;
        let diff = InlineDiff::for_document(&doc).unwrap();
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0].anchor, 0);
        assert!(diff.added.is_empty());
        // Hiding inline changes removes the reserved rows.
        let mut doc = doc;
        doc.inline_diff = false;
        assert!(InlineDiff::for_document(&doc).is_none());
    }
}
