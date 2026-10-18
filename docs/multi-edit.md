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

Only files whose content changed are written. For each:

1. The new content goes into a temp file next to the target, created
   exclusively. An existing file's permission bits are copied onto it; a new
   file gets `0666` minus the umask.
2. After every temp is staged, each target is checked again: still the same
   inode with the same bytes that were planned against (or, for a new file,
   still absent). Any change refuses the whole call and removes the temps.
3. Each temp is renamed onto its target. A new file is linked into place, so
   it never replaces a file that appeared meanwhile.

If a later file fails in step 3, the files already written are put back from
their in-memory originals (only if they still hold what this call wrote), and
the error lists which files were restored, which could not be, and which were
never written.

- A symlink stays a symlink: its target file is edited. A dangling symlink is
  an error.
- A file with more than one hard link is refused (replacing it would split the
  links).
- Only `append`/`append_jsonl` with `create: true` may create a file; the
  parent directory must exist.
- No fsync, matching `edit` and `write`: a crash can lose the latest change but
  does not leave a half-written file. The window between the re-check and the
  rename is not locked.

## Limits

Fixed, not configurable: 64 edits and 16 files per call, 16 MiB per file
(as read and after every edit), 1 MiB per `anchor`/`text`/`unless_present`/
`value` field (`text` and the `append_jsonl` row are checked again after
expansion), 64 MiB of file content per call. Files must be UTF-8.

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
