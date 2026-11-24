# Changes viewer

The changes viewer lists changed files in the file explorer and opens each one
as a normal, editable buffer that shows its changes inline. You can edit the
file, save it, and select lines to send to OpenCode, just as in any buffer.
The viewer never saves files, stages changes, or reverts edits by itself.

## Inline changes in any buffer

Every buffer that Git tracks has a diff base: the version it is compared with.
By default this is `HEAD`, which drives the change markers in the gutter and
`]g`/`[g` hunk navigation. With inline changes turned on:

- Added and changed lines have a dark green background across the whole row.
- Removed lines appear above the lines that replaced them, on a dark red
  background, syntax-highlighted with the buffer's language. They are drawn
  between real lines: the cursor skips them, and they cannot be selected,
  edited, or copied.
- The changes follow your edits as you type.

Commands:

| Command | Action |
| --- | --- |
| `:toggle-inline-diff` (`:inline-diff`) | Show or hide inline changes in the current buffer. |
| `:diff-base branch` | Compare the buffer with the file where the current branch split from the default branch. |
| `:diff-base head` | Compare the buffer with `HEAD` again. |
| `:diff-base` | Show what the buffer is compared with. |

`toggle_inline_diff` is also available as a command you can bind to a key.
Changing the diff base also changes the gutter markers and `]g`/`[g`. When the
statusline shows the `version-control` element, it includes the comparison, for
example `feature vs main (1a2b3c4d)`.

The default branch is `origin/HEAD` when the remote records one, otherwise
`main`, `master`, `origin/main`, or `origin/master`, in that order. The branch
point is `git merge-base HEAD <default branch>`. A file that did not exist at
the branch point is compared with an empty file.

To pick the background colors in a theme, set `diff.plus.background` and
`diff.minus.background`. Otherwise, Helix uses a dark shade of the theme's
`diff.plus` and `diff.minus` colors.

## Reviewing changes

Each review lists changed files in the explorer. Press `Enter` on a file to open
it in the current document view with inline changes against the review's
comparison. The explorer stays in review mode; press `Tab` to return to it.

| Command | Compares with |
| --- | --- |
| `:git-changes` | `HEAD`: staged, unstaged, and untracked changes. |
| `:branch-changes` | The branch point: commits on the branch plus uncommitted and untracked changes. |
| `:opencode-changes` | The files as they were before an OpenCode turn. |

From chat, a review uses the chat's session directory; from a document, it uses
the document workspace. Git operations honor workspace trust and disable
external diff drivers and text conversion.

Leaving review with `q` in the explorer, `:changes-close`, opening the normal
explorer, or closing the explorer turns off inline changes in the buffers the
review opened and compares them with `HEAD` again. Your edits stay. Starting
another review does the same for the previous review's buffers.

Opening a file keeps an already open buffer as it is, including unsaved
changes; it is never reloaded. Files that no longer exist, such as deleted
files, open in a read-only diff view instead, because there is no buffer to
show. Escaping symlinks and traversal paths are rejected.

### OpenCode turn changes

With a connected chat open, run `:opencode-changes`. Select a turn in the
searchable picker. Turns are listed in conversation order, oldest first, and
the selection starts on the latest turn. You can also select a loaded turn
anchor directly:

```text
:opencode-changes msg_example
```

The viewer requests OpenCode V2's `session.diff` for that turn with whole-file
patches and reads the file as it was before the turn from them. These are
server-recorded turn snapshots, not inferred tool changes. Because the buffer is
compared with its state before the turn, later turns and your own edits to the
same file also appear as changes.

OpenCode defines a turn from its first user prompt after idle through its next
idle marker. Steering prompts sent while busy belong to the same turn. Older
sessions without idle markers use one prompt per turn. The picker uses the
currently loaded chat history (up to the newest 200 messages).

Turn review uses the historical session location, including when a session has
since moved. If you close, replace, or reconnect the chat while the picker is
open, reopen the picker before selecting.

## Navigation

The changed-files list groups files into folders, sorted and indented like the
file explorer, with the same icons and status markers. The normal tree's
expansion and selection state is restored when you leave review.

| Location | Key | Action |
| --- | --- | --- |
| Explorer | `j` / `k`, page movement | Move through folders and files. |
| Explorer | `Enter` | Open the selected file with inline changes, or fold or unfold a folder. |
| Explorer | `l` / `h` | Unfold or fold a folder; `h` on a file or folded folder moves to its parent. |
| Explorer | `r` | Reload the list of changed files. |
| Explorer | `q` | Leave review and restore the normal file tree. |
| Document | `]g` / `[g` | Jump to the next or previous change. |
| Read-only diff | `o` | Open the current file, when it exists. |

## Limits

- Removed lines are drawn after the line that precedes them. Changes to the
  first line of a file show their removed lines after the change instead.
- Removed lines do not wrap; long lines are cut off at the edge of the view.
- Highlighting treats each block of removed lines on its own, so constructs that
  span unchanged lines (such as a long comment) can be colored differently.
- Git output, including whole-file patches, is limited to 2 MiB per command, and
  an OpenCode turn response is limited to 32 MiB and 10,000 files.
- A turn whose patch is not a single whole-file hunk, such as a binary file,
  opens in the read-only diff view.
- Local paths must correspond to the service's workspace. Remote path mapping is
  not available.
