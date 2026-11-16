# Frontend panes

The terminal frontend supports non-document panes through `ui::pane::Pane` and
`PaneManager`. The file explorer and OpenCode chat implement this interface. This is separate
from the compositor's modal layers: prompts and pickers temporarily overlay the
focused pane instead of participating in its layout.

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

The host currently supports docked panes, not arbitrary nesting of tool panes in
the document split tree. Each pane supplies a preferred width; narrow terminals
hide panes before taking space needed by documents.

## Input and commands

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
document insert mode. A pane can override `max_width` to reserve a different
portion of the docked area without changing document layout handling.

An `Overlay` outcome opens a compositor component for inspections or structured
interactions. OpenCode uses pane-local overlays for permission decisions, forms,
and diff review. These modals consume input rather than forwarding unhandled
keys to background documents. Opening or rendering an overlay must not imply
consent to a remote operation.

Palette actions run synchronously after the picker closes, using the picker's
compositor callback. This preserves commands that create prompts or enqueue
pane/focus callbacks without delaying editing commands through a background job.

## Adding a pane

1. Implement `Pane`: its kind, keymap, command handler, mouse handler, render,
   cursor, and optional refresh/update behavior.
2. Register it with `PaneManager::insert`, specifying a dock and preferred width.
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

- Custom panes dock on the left or right. They cannot be nested in the document
  split tree, split horizontally, or rearranged like document views.
- Preferred widths are specified in code and constrained by available space.
  There is no interactive pane resizing or rearranging.
- Narrow terminals hide panes and restore document focus rather than providing
  tabs, scrolling layouts, or another way to access hidden panes.
- Pane layout and content state are not persisted across sessions.

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
