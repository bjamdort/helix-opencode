# OpenCode chat

This first integration connects Helix to an existing OpenCode 2 service. It uses
the V2 HTTP API directly; it does not embed the OpenCode runtime or launch a TUI
subprocess. The chat pane uses the shared frontend pane host.

## Opening chat

- Press `Space o` to toggle the chat pane open or closed (on the left by default).
- Run `:opencode` to open or focus the pane without toggling it closed.
- Run `:opencode-sessions` to browse and attach an existing session by title.
- Closing chat with `Space o`, `q`, or `:quit` hides it. Reopening restores the
  same session, draft, staged snippets, detail setting, and history position.
  The connection stays active, so running work and approval requests continue
  to update while the panel is hidden. Hiding is not an interruption.
- Run `:opencode ses_...` to attach an existing session. The ID is validated before
  any request is made. An existing session keeps its server-side location.
- Opening a fresh chat creates a session at the current document's workspace
  directory. It uses the service's default agent and model.

Use `n` in the panel to start a new conversation or `:opencode-sessions` to switch.
Retention lasts for the current Helix process; it does not restore chat after
exiting and restarting Helix.

## Appearance

The panel recreates the OpenCode TUI session screen (`packages/tui`) inside the
pane, using the active Helix theme:

- There is no header. The transcript fills the pane and sticks to the bottom;
  when you scroll away, a `Jump to latest ↓` row appears above the prompt.
- User messages are shaded blocks with a `┃` rail in the agent color. Attached
  files and selections appear as ` file ` chips; queued prompts use a muted rail
  and a status line.
- Assistant text is indented Markdown. Consecutive tool calls and thoughts,
  including across steps, collapse into one summary such as
  `+ 2 commands, 1 edit, 1 thought, 3 reads`. While work runs, the summary shows
  a spinner and the current status, such as `Running command…`.
- Press `t` to expand summaries into the TUI's tool rows: `→ Read`, `✱ Grep`,
  `% WebFetch`, `◈ Web Search`, and subagent rows; shaded blocks for shell
  commands (tail-collapsed output), edits and patches (numbered diffs), written
  files, todos, and answered questions; and thoughts with their `**title**`.
  Denied tools are struck through, and failures show their error.
- A footer such as `Build · provider/model · 12.4s` ends each turn. Errors show
  as `Error: …`, and interrupted steps are marked. Compactions are full-width
  rules, and agent, model, and location switches are muted notices.
- Pending approvals and questions appear in a dock above the prompt. Press `p`
  to respond.
- The prompt is a shaded card with an agent-colored rail and a half-height lower
  edge. Below the input, it shows the agent, model, provider, and variant. The
  footer shows the session directory when idle, or an activity scanner with
  `c interrupt` while running, plus context tokens, cost, and key hints.

Duration, token, and cost figures come from the server's step records. Model
names show as `provider/model` because the pane does not load display names.
Spinners animate only while the session is running.

To customize the chat palette in a Helix theme, these optional scopes override
the indicated defaults:

| Scope | Default |
| --- | --- |
| `ui.opencode.accent` | `keyword` |
| `ui.opencode.agent` | `ui.opencode.accent`; rails, agent names, and the scanner |
| `ui.opencode.panel` | `ui.popup` background; user messages and tool blocks |
| `ui.opencode.prompt` | `ui.popup` background; the prompt card |
| `ui.opencode.border` | `ui.window`; queued and unfocused rails, thought rails |
| `ui.opencode.muted` | `ui.text.inactive` |
| `ui.opencode.thinking` | `warning`; thought titles |
| `ui.opencode.info` | `info` |
| `ui.opencode.heading` | `markup.heading` |
| `ui.opencode.bold` | `markup.bold`, with `constant` as its color fallback |
| `ui.opencode.code` | `markup.raw.inline` |
| `ui.opencode.list` | `function` |
| `ui.opencode.link` | `function`, underlined |
| `ui.opencode.success` | `string` |
| `ui.opencode.text` | `ui.text` |

Diffs use `diff.plus`, `diff.minus`, and `diff.delta`.

The service must already be running. Helix does not start, restart, replace, or
stop it. Check its availability with `opencode service status` if connection fails.

## Connection and authentication

By default Helix reads OpenCode's registration file:

```text
$XDG_STATE_HOME/opencode/service.json
```

When `XDG_STATE_HOME` is unset, the fallback is
`~/.local/state/opencode/service.json`, matching the OpenCode client. Helix reads
the advertised URL and private password, authenticates as `opencode`, and checks
the process/version identity through `GET /api/info`. Credentials are not printed
or copied into Helix configuration.

Environment overrides:

| Variable | Purpose |
| --- | --- |
| `HELIX_OPENCODE_SERVICE_FILE` | Use another local registration file. |
| `HELIX_OPENCODE_URL` | Connect to an explicit HTTP(S) server origin. |
| `HELIX_OPENCODE_PASSWORD` | Basic authentication password for the explicit URL. |

An explicit URL takes precedence over registration discovery. It never inherits
the local service password. Use HTTPS for non-local authenticated connections.
Credentials embedded in URLs, URL paths, queries, and fragments are rejected;
redirects are not followed. Remote servers need the workspace path to exist on
that server; there is no local-to-remote path mapping yet.

## Chat controls

| Key | Action |
| --- | --- |
| `i` | Edit the message field inside the chat pane; `Enter` submits it. |
| `Enter` outside input | Open a selected file reference, or compose when none is selected. |
| `Esc` in the prompt | Cancel composition, retaining the draft. |
| `j`, `k`, `Up`, `Down` | Scroll the transcript. |
| `gg`, `G`, `Home`, `End` | Go to the start or end of the visible history. |
| Page movement keys | Scroll by pages. |
| `c`, `Ctrl-c` | Request interruption of the session's current execution. |
| `n` | Start a new conversation. The previous session remains on the service. |
| `r` | Reconnect and reload authoritative session/message/inbox state. |
| `t` | Expand or collapse activity summaries into thoughts and tool rows. |
| `p` | Inspect pending permission requests and questions. |
| `D` | Review the latest turn's recorded file changes. |
| `J`, `K` | Jump to the next or previous message start. |
| `]`, `[` | Select the next or previous file reference and reveal it in the transcript. |
| `o` | Open the selected file reference; show the reference list when none is selected. |
| `f` | List file references from the loaded conversation. |
| `l`, `Right` / `h`, `Left` | Show / hide details. |
| `q`, `:q` | Close the pane, not the remote session. |
| `Esc`, `Tab` outside the prompt | Focus the last document pane. |

Shared bindings, including `:qa`, the command palette, and `Ctrl-w` window
navigation, remain available outside composition. Document-editing commands do
not operate on a buffer behind chat. Scrolling upward disables auto-follow;
scrolling to the bottom or sending a message enables it again.

## Session behavior

Assistant responses use Helix's Markdown renderer, including emphasis, lists, and
syntax-highlighted code when a matching runtime grammar is available. Wrapping
preserves inline styles.

The prompt previews your draft, up to three lines, when idle. During
composition, the shared Helix line editor and its cursor appear directly inside
the prompt, not on the global command line. `Enter` sends, and `Esc` cancels
while retaining the draft. The input follows pane layout changes and resizing.
Starting a new conversation clears the draft. Staged snippets appear as a chip
row at the top of the prompt.

## Selecting an existing session

Run `:opencode-sessions` to open a searchable session picker. You do not need an
open chat first. Type to filter by title, directory, or session ID. Non-archived
top-level sessions from all workspaces appear newest first; the currently
attached session is marked `current`.

Press `Enter` to attach the selected session or `Esc` to cancel. Browsing does
not create a session, send a prompt, change the working directory, or edit a
document. Sessions load in pages of 100 as the picker remains open, up to 10,000
sessions. Cancellation stops further pagination and never opens a delayed popup.

Before switching, Helix checks that the selected session still exists and uses
its current directory, even if the session moved after listing. If the current
chat has an unsent draft or staged snippet context, type `yes` in the confirmation
prompt to discard that local context and switch. `Esc` or any other answer keeps
the current chat. Selecting the already attached session focuses it without
discarding its draft. Switching does not interrupt work running in the old
server session.

You can still attach a known session directly with `:opencode ses_example`.
Archived and subagent sessions are omitted from the picker but can be attached
by ID.

## Sending editor selections with a message

Select text in a document, then run:

```text
:opencode-selection
```

Helix opens a `Selection comment >` input on the bottom row. Type a comment for
the selected code and press `Enter` to attach every captured selection with that
comment. An empty comment attaches the selection without an annotation. `Esc`
cancels without attaching anything or opening a new chat.

After you confirm the comment, the chat's message field opens. Type an overall
message if needed, then press `Enter` to send the snippets and comments together.
You can also prefill the overall message:

```text
:opencode-selection Explain this code
```

The bottom comment input always opens. To prefill it, use `--comment` (or `-c`):

```text
:opencode-selection --comment "Why can this return null?" Review the error handling
```

Confirm or edit the prefilled comment in the bottom row. The comment is attached
to each selection captured by that command. Add
selections in separate commands to give them different comments. Reattaching the
same snapshot with a new comment updates its annotation instead of duplicating
the reference.

To add, edit, or clear a comment after staging, run `:opencode-comment`. Select a
reference in the searchable popup, press `Enter`, and edit its comment. `Enter`
stores the comment; `Esc` leaves it unchanged. An empty comment clears the
annotation. Editing a reference comment never changes your overall message
draft or sends a prompt. Comments are limited to 16 KiB each.

Press `i` in chat to return to the message field and `Enter` to send. A
selection comment can also be sent without an overall chat message. Both the
desktop-compatible metadata and the model-facing prompt carry the comment;
metadata alone would not make it visible to the model.

The command reuses your existing chat or creates one in the document's workspace.
It does not submit a prompt until you confirm in the chat composer. `Esc` in that
composer keeps both the draft and its staged snippets. To add selections from other files, return to a
document, select text, and run the command again. Identical snapshots are
deduplicated. The composer shows the number of snippets and the first source
range.

Run `:opencode-clear-context` to remove all staged snippets without clearing the
message draft. A new chat clears both. A disconnected submission preserves
context; once the prompt is queued, its context belongs only to that prompt, not
the next one. Uncertain deliveries are not automatically retried.

Selections are immutable snapshots of the buffer, including unsaved edits.
Helix does not save or reload the file, read its contents from disk, or include
unselected text. Reversed and multiple selections work. Unnamed buffers are
attached as inline text without inventing a file path. At most 32 snippets and
1 MiB of selected UTF-8 text can be staged.

The wire format follows OpenCode V2's `PromptInput.FileAttachment`: `files`
contains inline data URIs, with `source` preserving the original file URI and
its one-based `start`/`end` line range. Message metadata uses the desktop's
`displayText`, `comments`, `preview`, and selection fields; character offsets are
zero-based UTF-16. Unlike a file-URI-only attachment, this preserves unsaved
buffer text and exact partial-line selections.

Sent attachments and their comments appear beneath your message and remain visible after history
reload. Named sources participate in file-reference navigation; opening them
uses the same workspace restrictions as other links.

## Selecting a model and variant

Run `:opencode-model` to open a searchable picker for the existing chat session.
The command works from a document or the chat pane. Open and connect a chat with
`:opencode` first.

Type to filter by provider, model ID, name, or variant. Each model has a row
without a variant and a row for each explicit variant, shown as
`provider/model#variant`. The row without `#variant` uses the model's default
settings; an explicit `#none` is a separate choice. The current selection is
marked `current`.

Press `Enter` to apply a selection, or `Esc` to cancel without changes. Only
enabled, non-deprecated chat models appear. Selecting a model changes subsequent
turns in this session, not global defaults or existing messages. Selection does
not send a prompt. The composer shows the selected model and variant.

If the chat closes, switches sessions, or reconnects while the picker is open,
reopen the picker before selecting. Failed requests are not automatically retried;
if the outcome is uncertain, reconnect to check the server's selection.

## Navigating chat and opening files

Use `j`/`k` and page movement to scroll normally, or `J`/`K` to move between
messages. File links, inline code paths, and plain paths in assistant Markdown
are underlined and navigable where they appear, including inside table-like text.
Use `]`/`[` to select one, then `Enter` or `o` to open it in the editor. Only the
selected link is highlighted, even when several links share a line. Selection
is highlighted and disables auto-follow while you inspect the conversation.
Manual scrolling clears reference selection, so `Enter` does not open a link
that you have scrolled away from.

Tool paths in expanded rows are links. Paths inside collapsed activity
summaries are still listed by `f`. The transcript does not append duplicate
file-reference rows.

Press `f` for a pane-local list of all references, including collapsed tools and
user messages. Use `j`/`k` to choose a file,
`Enter` to open it, or `Esc` to return to chat. Opening a reference focuses the
document view; `:opencode` brings you back to the existing chat. `Space o` closes
the pane if it is already open, even when a document has focus. Closing follows
the same behavior as `q`: it drops the local pane and draft, not the server session.

Supported references include Markdown file links, inline code paths, plain
path-like text, and paths supplied to file tools. Structured file outputs and
common grep/glob search results are also recognized; search output scanning is
bounded to its first 65,536 characters.

```text
[implementation](src/main.rs#L42)
src/main.rs:42
src/main.rs:42:7
src/main.rs#L42C7
src/main.rs#L42-L50
file:///workspace/src/main.rs#L42
```

Ranges open at their start. Line and column numbers are one-based; columns use
Helix grapheme positions. Out-of-range locations clamp to existing text. Paths
with spaces work in Markdown links or inline code; percent-encoded spaces in
links are decoded. A path without coordinates preserves an already-open buffer's
cursor position.

Relative paths resolve against the session directory, not the currently focused
document's directory. Only existing files inside that workspace are opened.
Missing files are not created, and traversal or symlinks escaping the workspace
are rejected. Absolute paths are accepted only inside the workspace. HTTP links
and other URI schemes are not opened as local files or executed. Remote path
mapping and references across session location changes are not yet supported.

Opening a reference never saves or reloads a buffer. Unsaved changes remain
intact, including when the on-disk file has changed. Coordinates are applied to
the buffer you actually see, which can differ from the agent's disk version.

Live text arrives over `GET /api/event`, filtered by session ID.
Completed text replaces its streamed draft. After an execution ends, Helix reloads
the session projection so the final transcript is authoritative.

The event stream is live-only. If it disconnects, Helix shows an error; press `r`
to rediscover the service and resynchronize. It does not automatically resend a
prompt. Each submitted prompt has a client-generated message ID. On a network
failure, delivery may already have happened: reconnect and inspect the transcript
and pending inputs before manually retrying.

Closing Helix, closing chat, or starting a new conversation disconnects the old
client worker but does not interrupt or delete its session. Use `c` to interrupt
explicitly. Sessions may continue running in the service after the pane closes.

OpenCode remains responsible for tools and permissions. Helix never auto-approves
requests or changes the session's permission rules. Tool execution can change
files on disk; Helix does not automatically reload dirty buffers.

## Approvals and questions

Press `p` to open the pane-local request list. Use `j`/`k` to select a request
and `Enter` to inspect it. Merely opening a request, pressing `Enter`, or
reconnecting never sends a decision.

For permissions, inspect the action, resources, directory, and tool metadata.
Use `j`/`k` or page movement to read long details. Press `o` to **allow once** or
`d` to **reject**. This version does not offer persistent "always allow" rules.

For questions:

- Use `j`/`k` to select a field and `Enter` to edit it inside the panel.
- Boolean fields toggle on `Enter` or `Space`.
- For choices, use `j`/`k` and `Enter` to choose; `Space` toggles multiselect
  options, and `Enter` returns to the form.
- If custom values are allowed, `e` in the choices view opens a text editor.
  Custom multiselect input uses a JSON array of strings.
- External fields display their URL but never launch it automatically. Complete
  the external step yourself, then press `Space` to acknowledge it.
- Press `s` to submit the complete form or `d` to cancel the server-side form.
  `Esc` returns without cancelling the server's question.

Defaults, required values, numeric bounds, option lists, and conditional field
visibility are respected. The server validates patterns and formats; rejected
answers leave the form pending and preserve your entered values for correction.
Hidden fields use supplied defaults and are not exposed as interactive controls.
Hidden required fields without defaults may require another client.

Press `r` in the request list to reload pending requests. If the connection is
lost, close the modal and press `r` in chat to reconnect. Pending requests reload
from the service. A response is never automatically retried. Before submitting,
Helix checks that the same request is still pending with unchanged details;
requests settled by another client are removed rather than replayed.

## Reviewing changes

To review a turn's files as editable buffers with inline changes, use
`:opencode-changes`. Use `:git-changes` or `:branch-changes` for local Git
changes and `:changes-close` to return to the normal tree. Because the buffers
are real, you can select changed lines and send them with `:opencode-selection`.
See [Changes viewer](changes.md) for controls and limits.

Press `D` to load the latest turn's structured diff from the service. Select a
file with `j`/`k` and press `Enter` to inspect its patch. Use `j`/`k` or page
movement to scroll; `r` refreshes the file list.

In a patch, `o` opens the existing local file in the editor. Opening a file never
automatically reloads or discards unsaved buffer contents. If an already-open
buffer is stale, decide whether to save or reload it yourself. Deleted files
remain reviewable as patches but cannot be opened from this view.

Review is read-only: there is no automatic patch application, rollback,
accept/reject, commit, or disk write. Only paths inside the session directory are
opened; traversal paths and symlinks escaping that directory are rejected.

## Current limits

- One chat pane at a time, with no tab bar. Use `:opencode-sessions` to browse
  existing sessions, or `:opencode` to attach by ID.
- Up to the newest 200 messages are loaded. Older-history pagination is not yet
  exposed in the UI.
- The composer uses a modal, single-line editor inside the pane, with horizontal
  scrolling for long input. It is not yet a multiline composer.
- Markdown uses Helix's existing renderer, not the complete OpenCode TUI renderer;
  file references have keyboard navigation, but web-link activation and rich
  tables are not yet supported.
- Tool details are bounded previews, not fully scrollable output inspectors:
  shell output keeps its last lines, and diffs and written files show up to 40
  lines. `t` expands every summary at once rather than one at a time.
- Diffs are unified with colored text; the TUI's split view and diff
  backgrounds are not reproduced. There is no sidebar.
- No agent selector, image uploads, file-reference insertion into prompts, or persistent
  client-side session selection yet.
- Turn selection is available through `:opencode-changes` for loaded history;
  staging, reverting, and other change-management mutations are not offered.
  Existing buffers are not automatically refreshed.
- Pane sizing follows the dock host; there is no interactive resizing.
- This is a connection/chat foundation, not a complete replacement for the
  OpenCode desktop interface yet.

## Live-service tests

Live tests are ignored by default. They discover the running service using the
same registration/authentication path as the pane; they never start or restart it.

```sh
# Connection, session creation, forms, staged inbox, and reconnect; no inference.
cargo test -p helix-term --lib live_service_connection_snapshot_forms_and_reconnect -- --ignored --nocapture

# Session model and variant switching, including default reset; no inference.
cargo test -p helix-term --lib live_service_model_and_variant_switching -- --ignored --nocapture

# Session listing, selection revalidation, and attachment; no inference.
cargo test -p helix-term --lib live_service_session_picker_listing_and_attach -- --ignored --nocapture

# Exact buffer snapshot, source range, and desktop metadata; resume=false, no inference.
cargo test -p helix-term --lib live_service_selected_snippet_admission -- --ignored --nocapture

# Two explicitly approved fixture edits; earlier-turn patches must remain historical.
HELIX_OPENCODE_TEST_MODEL='openai/gpt-6-luna#none' cargo test -p helix-term --lib live_service_historical_turn_changes -- --ignored --nocapture

# Streamed chat, history reload, reconnect, follow-up, and active interruption.
cargo test -p helix-term --lib live_service_chat_reconnect_and_followup -- --ignored --nocapture

# Explicit opt-in to GPT-6 Luna through OpenAI, with reasoning disabled.
HELIX_OPENCODE_TEST_MODEL='openai/gpt-6-luna#none' cargo test -p helix-term --lib live_service_chat_reconnect_and_followup -- --ignored --nocapture

# Isolated permissions, forms, reconnect, and another client answering first.
cargo test -p helix-term --lib live_service_permissions_forms_reconnect_and_other_client_resolution -- --ignored --nocapture

# Full task loop: an approved fixture edit, an agent question, and its diff.
HELIX_OPENCODE_TEST_MODEL='openai/gpt-6-luna#none' cargo test -p helix-term --lib live_service_edit_approval_question_and_diff -- --ignored --nocapture
```

The chat test defaults to `opencode/big-pickle`. Set
`HELIX_OPENCODE_TEST_MODEL` to another free OpenCode model ID if needed. Before
sending any prompt, the test checks the service's catalog: the model must be
enabled, active, a chat model, and explicitly zero-cost for every input, output,
and cache pricing tier. The only non-free exception is an explicit selection of
`openai/gpt-6-luna#none`, which uses your OpenAI account or plan with reasoning
disabled. The two completed replies are short; a counting reply is interrupted
as soon as execution starts. Tool permissions are denied. The test never
automatically falls back to a paid model. Model catalog availability
does not guarantee a valid provider login: if the service reports **Reconnect
OpenCode Console to continue**, reconnect that provider through `/connect` in
another OpenCode client and rerun the test. Local service authentication and
provider authentication are separate.

Tests use temporary directories under `/tmp/opencode` and their own sessions.
Chat-only inference tests deny all tools. The task-loop test allows a question
and a read of `marker.txt`, requests approval for editing only that fixture file,
and refuses any broader permission. Tests interrupt and delete
only their own sessions on completion or ordinary test failure paths.
Existing sessions and service configuration are not changed. If a test process
is forcibly terminated, its temporary session can remain on the service.

## Implementation and contract sources

- `helix-term/src/opencode/client.rs`: discovery, authentication, HTTP, SSE framing.
- `helix-term/src/opencode/mod.rs`: connection worker and transcript state.
- `helix-term/src/opencode/interactions.rs`: pending actions, validation, and stale-request checks.
- `helix-term/src/ui/opencode.rs`: pane content, composition, and rendering.
- `helix-term/src/ui/opencode/interactions.rs`: approval, question, and change-review views.
- `helix-term/src/ui/pane.rs`: shared layout, focus, and composition hosting.

The implementation follows the local OpenCode checkout's
`packages/protocol/openapi.json`, `packages/protocol/src/groups/`,
`packages/schema/src/session-message.ts`, `packages/schema/src/session-event.ts`,
`packages/client/src/service-probe.ts`, and `specs/v2/session.md`. The TUI's
`packages/tui/src/routes/session/message-parts.tsx` informs the presentation of
roles, reasoning, and tool activity. Public references:

- <https://opencode.ai/v2/docs/build/client>
- <https://opencode.ai/v2/docs/api>
