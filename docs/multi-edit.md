# `multi_edit`

Anchored edits across one or more files in a single tool call, all or
nothing. Every edit is checked against in-memory copies first; if any edit
fails, no file is written and the error names the edit.

`edit` is unchanged. Use `multi_edit` when a change touches several places or
files that must stay consistent, or when an anchor might match more than once.

## Call shape

```json
{
  "placeholders": false,
  "edits": [
    {"path": "/work/notes/STATUS.md", "op": "insert_before",
     "anchor": "## Overnight", "text": "## Today\n- started\n"},
    {"path": "/work/notes/log.jsonl", "op": "append_jsonl", "create": true,
     "value": {"event": "started"}, "timestamp_field": "ts"}
  ]
}
```

Per edit:

| Field | Ops | Meaning |
|---|---|---|
| `path` | all | File to change. Relative paths resolve against the session's working directory. |
| `op` | | `replace`, `insert_before`, `insert_after`, `append`, `append_jsonl` |
| `anchor` | replace, insert_* | What to find. Never expanded. |
| `anchor_mode` | replace, insert_* | `exact` (default), `line_prefix`, `line_regex` |
| `occurrence` | replace, insert_* | 1-based: use the Nth match instead of requiring exactly one |
| `text` | all but append_jsonl | Inserted or replacement text, verbatim |
| `value` | append_jsonl | The row, a JSON object |
| `timestamp_field` | append_jsonl | Field set to the call's clock (ISO 8601, local) |
| `unless_present` | all | Skip this edit if this text is already in the file |
| `create` | append, append_jsonl | Create the file if it is missing |

Unknown fields are refused, so a misspelled field is an error rather than
silently ignored. Unused optional fields may be omitted or `null`.

## Matching

- `exact`: substring match. Overlapping matches count (`aa` occurs twice in
  `aaa`).
- `line_prefix`: lines that start with the anchor. The span is the line
  without its terminator (`\n` or `\r\n`), so `replace` keeps the line ending,
  and `insert_after` inserts after the terminator (a `\n` is added after an
  unterminated last line first).
- `line_regex`: a regex tried on each line separately, so it never matches
  across a newline. With one capture group only the group is the span (use
  this to replace an unknown value inside a known line); more than one group
  is an error. `$1` in `text` is written literally, never expanded.

Every anchor must match exactly once, or `occurrence` must name an existing
match. Otherwise the call fails with the edit's index, the match count and the
matching line numbers.

Edits to the same file apply in order to the changing content, so a later
anchor can be text an earlier edit inserted. Line numbers in the result are as
of each edit.

`unless_present` is checked against the file's content at that point in the
call. A hit makes the edit a reported skip, not a failure, so re-running the
same call is safe.

## Placeholders

Off unless the call sets `"placeholders": true`. Then `text`, `unless_present`
and the string values inside `value` (not its keys) expand:

| Token | Value |
|---|---|
| `{{now_iso}}` | `%Y-%m-%dT%H:%M:%S%:z`, local time |
| `{{now:FMT}}` | local time in strftime `FMT` |
| `{{mtime_iso:/abs/path}}` | that file's modification time |
| `{{mtime:FMT:/abs/path}}` | the same in `FMT` (FMT ends at the first `:/`) |
| `{{{{` | a literal `{{` |

The clock is read once per call, so every edit and every `timestamp_field`
gets the same time. Each mtime path is read once per call. An unknown or
unterminated token, a bad strftime format, a relative mtime path or a missing
file is an error; nothing is ever left unexpanded silently. `}}` without an
opener is plain text.

## Writing

Only files whose content changed are written. During planning, each
target's parent directory is opened from the same canonical path the
write-scope check saw, by walking it from `/` without following symlinks.
Every later step (reading, staging, the re-check, publishing, restoring and
cleanup) works relative to that directory handle and never traverses the
parent path again.

Existing files are opened through the handle without following a symlink
and without waiting: a FIFO with no writer, or a device, is never blocked
on. Anything but a regular file is refused before a byte is read.

For each file:

1. The new content goes into a temp file next to the target, created
   exclusively. An existing file's temp is owner-only (`0600`) until every
   byte is written; only then does it get the file's permission bits. A new
   file's temp gets `0666` minus the umask, the mode it is published with.
2. After every temp is staged, and before anything is published, everything
   is checked again. Each parent path must still lead to the directory its
   handle holds (otherwise: "the parent directory of PATH was replaced").
   Each temp must still be the file this call created. Each target must
   still be the same inode with the same bytes that were planned against
   (or, for a new file, still absent). Any change refuses the whole call and
   removes this call's temps; a temp name taken over by someone else is left
   alone.
3. Each temp is renamed onto its target. A new file is published only by a
   primitive that never replaces an existing name: `renameat2` with
   `RENAME_NOREPLACE` on Linux, otherwise a hard link. On a filesystem with
   neither, the create is refused ("cannot create PATH without risking an
   overwrite on this filesystem"); there is no check-then-rename.

If a later file fails in step 3, the files already written are put back from
their in-memory originals (only if they still hold what this call wrote),
through owner-only temps like step 1, and the error lists which files were
restored, which could not be, and which were never written.

- A symlink stays a symlink: its target file is edited. A dangling symlink is
  an error.
- A file with more than one hard link is refused (replacing it would split the
  links).
- Only `append`/`append_jsonl` with `create: true` may create a file; the
  parent directory must exist.
- The written file is a new inode. Its permission bits are kept; its owner,
  group, ACLs and extended attributes are not (they are those of a new file
  made by this process), the same as with `edit` and `write`. This holds for
  a file put back by a restore too.
- No fsync, matching `edit` and `write`: a crash can lose the latest change but
  does not leave a half-written file.

Best effort, not a transaction:

- The window between the re-check and the renames is not locked. A write by
  another process in that window can be overwritten. A parent directory
  moved in that window still receives the file, through its handle, where
  it now is.
- A symlink given as the path and switched to another target after planning
  does not change what is written: the target resolved during planning is
  the one edited.
- A restore has its own unlocked window between reading a file back and
  putting its original back. Each file's outcome is reported exactly.
- Nothing is atomic across files or across a crash.

On systems other than Unix there are no directory handles: every step works
by path, as before, the parent re-check does not apply, and a new file is
published by hard link only.

## Limits

Fixed, not configurable: 64 edits and 16 files per call, 16 MiB per file
(as read and after every edit), 1 MiB per `anchor`/`text`/`unless_present`/
`value` field (`text` and the `append_jsonl` row are checked again after
expansion), 64 MiB of existing files read per call and 64 MiB of content
written per call. The read limits are checked from each file's size before
it is read, and at most one byte past a limit is read, so a file that grows
in between is still refused. These are input limits, not a bound on the
process's memory: planning holds each file's original and edited copies and
the anchor matches. Files must be UTF-8.

## Classification

- Effect `NonIdempotent` with a serializing concurrency key. Unlike `edit`,
  an interrupted call is never re-run automatically, because an insert or
  append would land twice.
- In the progressive-disclosure core next to `edit`, and available to workers
  like `edit`.
- While a worker holds a write scope, a foreground call is refused if any
  path, as given, as resolved, or as its real target with symlinks followed,
  overlaps that scope.
- Compaction lists every `edits[].path` among the modified files.

## Hooks

Extension `before_tool_call` / `after_tool_call` hooks with a `tool` filter
match by tool name. A hook registered for `edit` or `write` does not see
`multi_edit` calls; add `{"hook": "before_tool_call", "tool": "multi_edit"}`
to cover it. The arguments are plain fields (`edits[].path`, `op`, `anchor`,
`text`) so a hook can summarise them per file.
