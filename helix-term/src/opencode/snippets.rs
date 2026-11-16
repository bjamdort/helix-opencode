//! Immutable editor selections transported with the V2 PromptInput file schema.
use std::{fmt::Write, path::PathBuf};

use anyhow::{Context, Result};
use helix_view::Editor;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) const MAX_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_SNIPPETS: usize = 32;
pub(crate) const MAX_COMMENT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Selection {
    pub start_line: usize,
    pub start_char: usize,
    pub end_line: usize,
    pub end_char: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snippet {
    pub path: Option<PathBuf>,
    pub text: String,
    pub selection: Selection,
    pub comment: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Attachment {
    pub uri: String,
    pub source: Value,
    pub name: String,
    pub description: String,
}

impl Snippet {
    pub fn same_source(&self, other: &Self) -> bool {
        self.path == other.path && self.text == other.text && self.selection == other.selection
    }

    /// Match the desktop's formatCommentNote / formatQuoteComment: metadata is
    /// presentation-only, so the model must also receive the annotation in text.
    fn comment_text(&self) -> Option<String> {
        let comment = self.comment.trim();
        if comment.is_empty() {
            return None;
        }
        Some(match &self.path {
            Some(path) => {
                let range = if self.selection.start_line == self.selection.end_line {
                    format!("line {}", self.selection.start_line)
                } else {
                    format!("lines {} through {}", self.selection.start_line, self.selection.end_line)
                };
                format!("The user made the following comment regarding {range} of {}: {comment}", path.display())
            }
            None => format!(
                "The user quoted the following from earlier in the conversation:\n{}\n\nThe user made the following comment regarding this quote: {comment}",
                self.text.split('\n').map(|line| format!("> {line}")).collect::<Vec<_>>().join("\n")
            ),
        })
    }

    pub fn label(&self) -> String {
        let name = self
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled selection".into());
        format!(
            "{name}:{}-{}",
            self.selection.start_line, self.selection.end_line
        )
    }

    pub fn attachment(&self) -> Result<Attachment> {
        // Percent-encoding keeps exact UTF-8 bytes, including newlines and '+'.
        let mut uri = String::from("data:text/plain;charset=utf-8,");
        for byte in self.text.bytes() {
            write!(uri, "%{byte:02X}")?;
        }
        let source = if let Some(path) = &self.path {
            let mut url = reqwest::Url::from_file_path(path)
                .map_err(|_| anyhow::anyhow!("Cannot construct selected file's URI"))?;
            url.query_pairs_mut()
                .append_pair("start", &self.selection.start_line.to_string())
                .append_pair("end", &self.selection.end_line.to_string());
            json!({"type":"uri", "uri": url.as_str()})
        } else {
            json!({"type":"inline"})
        };
        Ok(Attachment {
            uri,
            source,
            name: self
                .path
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "selection.txt".into()),
            description: format!(
                "Editor selection from {} (buffer snapshot, not a disk read)",
                self.label()
            ),
        })
    }

    pub fn comment(&self) -> Value {
        match &self.path {
            Some(path) => json!({
                "path":path, "selection":self.selection, "comment":self.comment.trim(),
                "preview":self.text, "origin":"file"
            }),
            None => {
                let mut quote = json!({"type":"quote","text":self.text});
                if !self.comment.trim().is_empty() {
                    quote["comment"] = json!(self.comment.trim());
                }
                quote
            }
        }
    }
}

pub(crate) fn capture(editor: &Editor) -> Result<Vec<Snippet>> {
    anyhow::ensure!(
        editor.focused_pane.is_none(),
        "Focus an editor document before selecting snippets"
    );
    let (view, doc) = helix_view::current_ref!(editor);
    let text = doc.text().slice(..);
    let selection = doc.selection(view.id);
    anyhow::ensure!(
        selection.len() <= MAX_SNIPPETS,
        "Select at most {MAX_SNIPPETS} snippets"
    );
    let mut bytes = 0;
    let mut snippets = Vec::new();
    for range in selection {
        let start = range.from();
        let end = range.to();
        if start == end {
            continue;
        }
        let slice = text.slice(start..end);
        bytes += slice.len_bytes();
        anyhow::ensure!(
            bytes <= MAX_BYTES,
            "Selected snippets exceed the 1 MiB limit"
        );
        let start_row = text.char_to_line(start);
        let end_row = text.char_to_line(end - 1);
        let end_at = if text.char_to_line(end) == end_row {
            end
        } else {
            // A selection ending at the next line's start belongs to the previous line.
            let line = text.line(end_row).to_string();
            text.line_to_char(end_row) + line.trim_end_matches(['\r', '\n']).chars().count()
        };
        let utf16_column = |row, at| {
            text.slice(text.line_to_char(row)..at)
                .chars()
                .map(char::len_utf16)
                .sum()
        };
        snippets.push(Snippet {
            path: doc.path().map(ToOwned::to_owned),
            text: slice.to_string(),
            comment: String::new(),
            selection: Selection {
                start_line: start_row + 1,
                end_line: end_row + 1,
                start_char: utf16_column(start_row, start),
                end_char: utf16_column(end_row, end_at),
            },
        });
    }
    anyhow::ensure!(
        !snippets.is_empty(),
        "Select some text before adding a snippet"
    );
    // Validate URI construction before changing any composer state.
    for snippet in &snippets {
        snippet
            .attachment()
            .context("Cannot attach editor selection")?;
    }
    Ok(snippets)
}

pub(crate) fn prompt(id: &str, text: &str, snippets: &[Snippet]) -> Result<Value> {
    let mut body = json!({"id":id,"text":text});
    if !snippets.is_empty() {
        anyhow::ensure!(
            snippets
                .iter()
                .all(|snippet| snippet.comment.len() <= MAX_COMMENT_BYTES),
            "Selection comments exceed the 16 KiB limit"
        );
        body["text"] = json!(std::iter::once(text.to_owned())
            .filter(|text| !text.trim().is_empty())
            .chain(snippets.iter().filter_map(Snippet::comment_text))
            .collect::<Vec<_>>()
            .join("\n"));
        body["files"] = serde_json::to_value(
            snippets
                .iter()
                .map(Snippet::attachment)
                .collect::<Result<Vec<_>>>()?,
        )?;
        body["metadata"] = json!({
            "displayText":text, "comments":snippets.iter().map(Snippet::comment).collect::<Vec<_>>(),
            "attachments":[]
        });
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippet(path: Option<&str>, comment: &str) -> Snippet {
        Snippet {
            path: path.map(PathBuf::from),
            text: "selected code".into(),
            selection: Selection {
                start_line: 4,
                end_line: 6,
                start_char: 0,
                end_char: 2,
            },
            comment: comment.into(),
        }
    }

    #[test]
    fn reference_comments_reach_the_model_and_desktop_metadata_separately_from_the_message(
    ) -> Result<()> {
        let first = snippet(Some("/work/first.rs"), "  Handle this error  ");
        let mut second = snippet(Some("/work/second.rs"), "Explain the return value");
        second.selection.end_line = 4;
        let request = prompt("msg_test", "Review both snippets", &[first, second])?;
        assert_eq!(request["metadata"]["displayText"], "Review both snippets");
        assert_eq!(
            request["metadata"]["comments"][0]["comment"],
            "Handle this error"
        );
        assert_eq!(
            request["metadata"]["comments"][1]["comment"],
            "Explain the return value"
        );
        assert_eq!(request["text"], "Review both snippets\nThe user made the following comment regarding lines 4 through 6 of /work/first.rs: Handle this error\nThe user made the following comment regarding line 4 of /work/second.rs: Explain the return value");
        assert_eq!(request["files"].as_array().unwrap().len(), 2);
        Ok(())
    }

    #[test]
    fn quote_comments_prefix_each_quoted_line_and_can_supply_the_entire_message() -> Result<()> {
        let mut snippet = snippet(None, "What is wrong here?");
        snippet.text = "first\n> nested\nlast".into();
        let request = prompt("msg_quote", "", &[snippet])?;
        assert_eq!(request["metadata"]["displayText"], "");
        assert_eq!(
            request["metadata"]["comments"][0]["comment"],
            "What is wrong here?"
        );
        assert_eq!(request["text"], "The user quoted the following from earlier in the conversation:\n> first\n> > nested\n> last\n\nThe user made the following comment regarding this quote: What is wrong here?");
        Ok(())
    }

    #[test]
    fn empty_comments_preserve_plain_prompts_and_oversized_comments_are_rejected() -> Result<()> {
        let mut snippet = snippet(Some("/work/source.rs"), " ");
        let request = prompt(
            "msg_test",
            "Original prompt",
            std::slice::from_ref(&snippet),
        )?;
        assert_eq!(request["text"], "Original prompt");
        assert_eq!(request["metadata"]["comments"][0]["comment"], "");
        snippet.comment = "x".repeat(MAX_COMMENT_BYTES + 1);
        assert!(prompt("msg_test", "Original prompt", &[snippet]).is_err());
        Ok(())
    }
}
