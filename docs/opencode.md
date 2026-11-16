# OpenCode chat

This first integration connects Helix to an existing OpenCode 2 service. It uses
the V2 HTTP API directly; it does not embed the OpenCode runtime or launch a TUI
subprocess. The chat pane uses the shared frontend pane host.

## Opening chat

- Press `Space o` to toggle the right-hand chat pane open or closed.
- Run `:opencode` to open or focus the pane without toggling it closed.
- Run `:opencode ses_...` to attach an existing session. The ID is validated before
  any request is made. An existing session keeps its server-side location.
- Opening a fresh chat creates a session at the current document's workspace
  directory. It uses the service's default agent and model.

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
| `t` | Toggle reasoning and tool details. |
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

The pane follows the OpenCode TUI's layout: a session header, an independently
scrolling conversation, and a persistent composer card with agent/model metadata.
User messages have a shaded background and accent rail; assistant responses use
Helix's Markdown renderer, including emphasis, lists, and syntax-highlighted code
when a matching runtime grammar is available. Wrapping preserves inline styles.

The composer previews your draft when idle. During composition, the shared Helix
line editor and its cursor appear directly inside the message field, not on the
global command line. `Enter` sends, and `Esc` cancels while retaining the draft.
The input follows pane layout changes and resizing. Starting a new conversation
clears the draft.
Tool rows show status icons and input summaries. Press `t` to show reasoning and
bounded tool input/output previews. Errors and approval notices remain visible
in the transcript. Colors follow your Helix theme.

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

Completed consecutive read tools collapse into a `Read N files` summary. Press
`t` to expand their individual entries and previews; running or failed reads
remain visible. The transcript does not append duplicate file-reference rows.

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

- One chat pane at a time; no session browser or tab bar. Existing sessions can be
  selected by ID with `:opencode`.
- Up to the newest 200 messages are loaded. Older-history pagination is not yet
  exposed in the UI.
- The composer uses a modal, single-line editor inside the pane, with horizontal
  scrolling for long input. It is not yet a multiline composer.
- Markdown uses Helix's existing renderer, not the complete OpenCode TUI renderer;
  file references have keyboard navigation, but web-link activation and rich
  tables are not yet supported.
- Tool details are bounded previews, not fully scrollable output inspectors.
- No agent selector, image uploads, file-reference insertion into prompts, or persistent
  client-side session selection yet.
- Review covers the latest turn, not arbitrary turn selection or full change
  management. Existing buffers are not automatically refreshed.
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

# Exact buffer snapshot, source range, and desktop metadata; resume=false, no inference.
cargo test -p helix-term --lib live_service_selected_snippet_admission -- --ignored --nocapture

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
