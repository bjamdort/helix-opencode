//! Explicit command capabilities. Unknown commands are document-only by default.
use super::{Context, MappableCommand};
use crate::{compositor, ui::EditorView};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneCommand {
    Up,
    Down,
    Left,
    Right,
    First,
    Last,
    PageUp,
    PageDown,
    Activate,
    OpenHorizontalSplit,
    OpenVerticalSplit,
    Refresh,
    Close,
    FocusDocument,
    Compose,
    Interrupt,
    New,
    ToggleDetails,
    Requests,
    Review,
    NextReference,
    PreviousReference,
    OpenReference,
    References,
    NextMessage,
    PreviousMessage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandScope {
    Workspace,
    Pane(PaneCommand),
    Document,
}

pub(crate) fn scope(command: &MappableCommand) -> CommandScope {
    use CommandScope::*;
    if matches!(
        command,
        MappableCommand::Typable { .. } | MappableCommand::Macro { .. }
    ) {
        // Typed commands perform their own capability check after resolving aliases.
        // Macros re-enter key handling, so every expanded command is checked again.
        return Workspace;
    }
    match command.name() {
        "move_line_up" | "move_visual_line_up" => Pane(PaneCommand::Up),
        "move_line_down" | "move_visual_line_down" => Pane(PaneCommand::Down),
        "move_char_left" => Pane(PaneCommand::Left),
        "move_char_right" => Pane(PaneCommand::Right),
        "goto_file_start" => Pane(PaneCommand::First),
        "goto_last_line" | "goto_file_end" => Pane(PaneCommand::Last),
        "page_up" | "half_page_up" | "page_cursor_up" | "page_cursor_half_up" => {
            Pane(PaneCommand::PageUp)
        }
        "page_down" | "half_page_down" | "page_cursor_down" | "page_cursor_half_down" => {
            Pane(PaneCommand::PageDown)
        }
        "pane_activate" => Pane(PaneCommand::Activate),
        "pane_open_horizontal_split" => Pane(PaneCommand::OpenHorizontalSplit),
        "pane_open_vertical_split" => Pane(PaneCommand::OpenVerticalSplit),
        "pane_compose" => Pane(PaneCommand::Compose),
        "pane_interrupt" => Pane(PaneCommand::Interrupt),
        "pane_new" => Pane(PaneCommand::New),
        "pane_toggle_details" => Pane(PaneCommand::ToggleDetails),
        "pane_requests" => Pane(PaneCommand::Requests),
        "pane_review" => Pane(PaneCommand::Review),
        "pane_next_reference" => Pane(PaneCommand::NextReference),
        "pane_previous_reference" => Pane(PaneCommand::PreviousReference),
        "pane_open_reference" => Pane(PaneCommand::OpenReference),
        "pane_references" => Pane(PaneCommand::References),
        "pane_next_message" => Pane(PaneCommand::NextMessage),
        "pane_previous_message" => Pane(PaneCommand::PreviousMessage),
        "pane_refresh" => Pane(PaneCommand::Refresh),
        "pane_close" => Pane(PaneCommand::Close),
        "focus_document_pane" => Pane(PaneCommand::FocusDocument),
        "no_op"
        | "command_mode"
        | "command_palette"
        | "opencode_chat"
        | "file_explorer"
        | "file_explorer_in_current_buffer_directory"
        | "file_explorer_in_current_directory"
        | "toggle_file_explorer"
        | "file_picker"
        | "file_picker_in_current_directory"
        | "file_picker_in_current_buffer_directory"
        | "buffer_picker"
        | "changed_file_picker"
        | "last_picker"
        | "rotate_view"
        | "rotate_view_reverse"
        | "jump_view_left"
        | "jump_view_right"
        | "jump_view_up"
        | "jump_view_down"
        | "wclose"
        | "record_macro"
        | "replay_macro"
        | "suspend" => Workspace,
        _ => Document,
    }
}

pub(crate) fn requires_document(name: &str) -> String {
    format!("'{name}' requires a document pane")
}

/// Preserve the pane identity captured at dispatch, rather than using whichever
/// pane happens to be focused when the compositor callback runs.
pub(crate) fn dispatch(cx: &mut Context, command: PaneCommand) {
    let target = cx.editor.focused_pane;
    let count = cx.count();
    cx.callback.push(Box::new(move |compositor, cx| {
        if cx.editor.focused_pane != target {
            return;
        }
        if let Some(view) = compositor.find::<EditorView>() {
            let callback = view.panes.command(command, count, cx.editor);
            if let Some(callback) = callback {
                callback(compositor, cx);
            }
        }
    }));
}

pub(crate) fn close_callback(target: helix_view::PaneId) -> compositor::Callback {
    Box::new(move |compositor, cx| {
        if cx.editor.focused_pane != Some(target) {
            return;
        }
        if let Some(view) = compositor.find::<EditorView>() {
            let _ = view.panes.command(PaneCommand::Close, 1, cx.editor);
        }
    })
}

/// Canonical typed command names, checked after alias resolution. Everything
/// else requires an explicit document focus; even read-only document commands
/// must not silently operate on a background buffer.
pub(crate) fn typed_is_workspace(name: &str) -> bool {
    matches!(
        name,
        "quit-all"
            | "quit-all!"
            | "cquit"
            | "cquit!"
            | "write-all"
            | "write-all!"
            | "write-quit-all"
            | "write-quit-all!"
            | "open"
            | "new"
            | "theme"
            | "config-reload"
            | "config-open"
            | "config-open-workspace"
            | "log-open"
            | "set-option"
            | "toggle-option"
            | "get-option"
            | "redraw"
            | "echo"
            | "noop"
            | "change-current-directory"
            | "show-directory-stack"
            | "push-directory"
            | "pop-directory"
            | "show-directory"
            | "show-clipboard-provider"
            | "workspace-trust"
            | "workspace-untrust"
            | "workspace-exclude"
            | "lsp-restart"
            | "lsp-stop"
            | "lsp-workspace-command"
            | "clear-register"
            | "set-register"
            | "run-shell-command"
            | "opencode"
            | "opencode-model"
            | "opencode-sessions"
            | "git-changes"
            | "branch-changes"
            | "opencode-changes"
            | "changes-close"
            | "opencode-clear-context"
            | "opencode-comment"
    )
}
