# Frontend panes

The terminal frontend supports non-document panes through `ui::pane::Pane` and
`PaneManager`. The file explorer and OpenCode chat implement this interface. This is separate
from the compositor's modal layers: prompts and pickers temporarily overlay the
focused pane instead of participating in its layout.

Changes review opens changed files as normal buffers with inline changes (see
[Changes viewer](changes.md)). Only files without a buffer to open, such as
deleted files, use `Dock::Center`: a read-only pane over the document region
that does not change document buffers or split geometry. The file explorer
shares a review snapshot with that pane and filters its list to changed files.
The bufferline belongs to the document region, below any top-docked tools and
beside any side-docked tools. It does not span chat or the explorer. Diff review
covers the whole document region, including the bufferline. While it is visible,
document views, statuslines, and the bufferline are not drawn.
When a document has focus, the central pane does not render or receive mouse
events, so documents cannot be edited behind a covering review surface.

## Configuring layout

The layout has two groups: OpenCode occupies a percentage of the entire editor
area, and the workspace occupies the remainder. Inside the workspace, the file
explorer occupies its own percentage, and document views share the remainder.
The changed-files list uses the explorer's placement and size. The read-only
diff review continues to cover the document region, not the explorer or chat.

These are the defaults in `config.toml`:

```toml
[editor.panes]
opencode-position = "left"
opencode-size = 40
file-explorer-position = "left"
file-explorer-size = 25
```

This gives `OpenCode | [explorer | editor]`, sized `40% | 60% [25% | 75%]`.
With all panes visible, their total widths are 40%, 15%, and 45%, respectively.
Percentages round down to whole terminal cells; the remainder goes to the inner
group. Panes open in the same positions regardless of opening order.

The host reserves one cell inside each tool pane for the separator facing its
neighbor: right for left-docked panes, left for right-docked panes, bottom for
top-docked panes, and top for bottom-docked panes. Content, mouse coordinates,
and composer anchors use the remaining interior. Outer terminal edges do not
receive a separator.

Each position accepts `left`, `right`, `top`, or `bottom`. Sizes are integer
percentages from 1 to 99, relative to the containing group. Left/right positions
measure width; top/bottom positions measure height. For example, setting
`opencode-position = "bottom"` and `opencode-size = 30` reserves the bottom 30%
for chat, leaving the workspace above it.

Closing chat gives the workspace the full editor area. Closing the explorer
gives documents the entire workspace. These options do not open panes
automatically. Run `:config-reload` after editing your configuration; the next
layout uses the new values without reopening panes or modifying buffers.

If a split would leave fewer than eight columns for a tool pane or 20 columns
for its remaining group (left/right), or fewer than four rows for either side
(top/bottom), the auxiliary pane is temporarily hidden. Resizing the
terminal restores it when there is enough room. A hidden pane loses focus to
the document. There is no mouse-drag resizing or arbitrary layout tree yet.

## Ownership and focus

`EditorView` owns one `PaneManager`, which stores pane implementations behind
stable `PaneId` keys. The host reserves docked areas and gives the remaining
rectangle to the existing document split tree. Document views retain their
existing `ViewId`, document state, and layout implementation; they are not fake
tool buffers.

`Editor::focused_pane` is the single source of auxiliary-pane focus. When it is
`None`, `Editor::tree.focus` identifies the active document. While an auxiliary
pane is focused, the document focus remains the destination for file opening.
Window navigation and rotation consider both kinds of target. Hiding or closing
the focused pane restores document focus. Switching panes clears pending keymap
state, counts, sticky menus, next-key callbacks, and completion state.

Panes can opt into `retain_on_close`. The host then hides their slot instead of
dropping the pane; hidden panes receive no layout, render, mouse, or navigation
targets. Focusing a retained pane reveals the same instance. `on_hide` clears
transient anchors without discarding content. OpenCode uses this to preserve its
session, streaming connection, draft, attachments, and scroll state when closed.

The host allocates the outer group before the workspace group, independent of
pane insertion order. Neither group becomes part of the document split tree.
Narrow terminals hide panes before taking space needed by documents.

## Input and commands

In the file explorer, `Enter` opens the selected file in the active document
window. `\` opens it in a horizontal split below that window, and `|`
(Shift+`\` on keyboards where it produces `|`) opens it in a vertical split to
the right. Focus moves to the opened file; the explorer, other splits, and unsaved
buffers remain intact. On a directory, these keys toggle expansion instead of
creating an empty split.

The editor's existing `Keymaps` engine resolves normal-mode bindings with the
focused pane's keymap overlaid. Shared bindings, user remappings, prefixes, counts,
macros, and help menus go through the same path as document input. Pane-specific
bindings take precedence at conflicting keys; this does not introduce a separate
configuration section for each pane type.

`commands::pane` classifies resolved commands as workspace-wide, pane actions, or
document-only. The default is document-only, so new commands do not accidentally
gain access to a background document. `MappableCommand::execute` enforces this
boundary for bindings, command sequences, macros, and palette actions. Typed
commands check capabilities after resolving aliases, including commands bound
directly to keys. `:quit` closes the focused pane; `:quit-all` retains normal
application shutdown and unsaved-buffer checks.

Pane actions translate shared movement commands into content-specific behavior.
The host handles file-open, close, and focus outcomes. Panes must not decode
global key prefixes or keep their own focus flags. Bracketed paste does not fall
through to a document when an auxiliary pane has focus.

Panes can also return a `Compose` outcome with a shared draft and an optional
layout anchor. The host opens a Helix line editor inside that anchor (or on the
command line when no anchor is provided) and routes validated input back to
`Pane::submit`; cancellation and failed submission retain the draft. This keeps chat composition separate from
document insert mode. Layout belongs to the host rather than content-specific
width caps.

An `Overlay` outcome opens a compositor component for inspections or structured
interactions. OpenCode uses pane-local overlays for permission decisions, forms,
and diff review. These modals consume input rather than forwarding unhandled
keys to background documents. Opening or rendering an overlay must not imply
consent to a remote operation.

A `Callback` outcome supports synchronous compositor actions after pane command
dispatch, such as opening the central diff pane or restoring the normal explorer.
Pane content handles close and document-focus commands explicitly, so a filtered
view can leave its mode without discarding the hosting explorer.

Palette actions run synchronously after the picker closes, using the picker's
compositor callback. This preserves commands that create prompts or enqueue
pane/focus callbacks without delaying editing commands through a background job.

## Adding a pane

1. Implement `Pane`: its kind, keymap, command handler, mouse handler, render,
   cursor, and optional refresh/update behavior.
2. Register it with `PaneManager::insert`, specifying an outer, workspace, or
   center dock. The layout configuration controls the dock's position and size.
3. Use `PaneOutcome` to request opening a file, closing, or returning to a document.
4. Opt additional shared commands into `commands::pane` where appropriate. Do not
   forward unknown input to document editing.
5. Capture the `PaneId` in background work and resolve it through the host when
   applying results. A removed pane's ID never identifies its replacement.

No explorer-specific changes to global event routing or layout are needed for
another pane implementation. Commands that require document state intentionally
remain unavailable until document focus is restored.

## Current limitations

This is a reusable docked-pane system, not a general-purpose window system. It
provides a foundation for explorers, outlines, and similar navigation panels;
terminals or arbitrary mixed-content splits need further architectural work.

### Layout

- Tool panes occupy outer/workspace groups with configurable edges and sizes.
  They cannot be nested in arbitrary groups or rearranged like document views.
- There is no interactive pane resizing or rearranging.
- Narrow terminals hide panes and restore document focus rather than providing
  tabs, scrolling layouts, or another way to access hidden panes.
- Configured layout persists in `config.toml`; open-pane and content state are
  not persisted across sessions.

### Keybindings and modes

- Auxiliary panes use normal-mode bindings plus a local overlay. They do not
  have independent editing modes or a general mode-management interface.
- Pane-specific bindings are defined in code. There is no dedicated per-pane
  keybinding configuration section, and overlay bindings take precedence over
  conflicting normal-mode remappings.
- Inherited prefix menus can display document-only commands. Those commands
  report that they require a document pane; the command palette filters them
  out when an auxiliary pane has focus.

### Command capabilities

- Command classification is maintained explicitly in `commands::pane`. A new
  command defaults to document-only until it is classified otherwise.
- Workspace commands are explicitly allowed, not automatically inferred to be
  safe in every pane. Adding a shared command requires reviewing its assumptions
  about document state and focus.
- `PaneCommand` and `PaneOutcome` are predefined enums. New interactions that
  do not fit existing navigation actions or outcomes require extending the host
  interfaces, not only implementing a new pane.

### Document and focus assumptions

- The editor still requires a document view. Auxiliary panes cannot keep the
  application open independently after the last document view closes.
- The last document view remains the file-open destination and supplies context
  for some workspace operations. Auxiliary panes do not have a complete,
  independent workspace or command context.
- Focus management bridges the existing document split tree and the auxiliary
  pane host. Future navigation, closing, and layout operations must account for
  both systems; document-tree operations alone do not manage custom panes.

### Extensibility

- This is an internal Rust interface, not a plugin API. Adding a pane requires
  source changes and rebuilding Helix.
- A terminal pane, arbitrary mixed document/tool splits, or pane-specific editing
  modes would require revisiting input ownership, layout, command capabilities,
  and application lifetime. These should not be implemented by forwarding
  unrecognized keys to the background document or adding explorer-specific
  exceptions to shared routing.
