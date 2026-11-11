# Context archive files

When a chat gets too long, Synaps seals the older messages of its context
window into an **archive file** and the chat carries on in a fresh window.
That is a rollover. Archive files are plain JSON, and an extension may read
them directly: to index them, to search them, to bring something back into a
conversation.

This page is the contract for those files. It is written so that you can
build a reader in any language without reading the Synaps source. It is kept
true by tests (`crates/agent-core/tests/context_archive_format.rs`) that run
against real files, which you can use too:

- fixtures: [`fixtures/context-archives/`](fixtures/context-archives/), one
  project directory exactly as Synaps wrote it;
- a reference reader in Python, standard library only:
  [`context_archive_reader.py`](../../examples/extensions/context_archive_reader.py),
  printed in full [below](#a-reference-reader).

What this page covers:

- **The built-in file store only.** That is `memory.backend = legacy`, the
  default. With `memory.backend = axel` no archive files are written.
- **Unix only, today.** The file store exists on Linux and macOS. On other
  platforms Synaps writes no archives, and nothing here describes them.
- **Synaps writes, you read.** Reading is the only thing an extension may do
  with these files.
- **Format version 1.** Every file says which version it is. There is one
  version so far.

## In short

1. Find the project directory. The `path` in the
   [`on_context_archived`](hooks.md#on_context_archived) event leads to it.
2. In that directory, read the files named `000.json` to `127.json`. In each
   of its subdirectories named `s` and exactly six digits, like `s000001`,
   read the files named `<id>.json`, where `<id>` is 32 lowercase hex
   characters. Ignore every other name at both levels, and anything under
   one of these names that is a link.
3. A file is one JSON object. Look at `version` first; if it is not `1`, skip
   the file and go on with the next.
4. Skip the file if `scope` is not the name of the project directory or, in a
   shelf, if `id` is not the file's name.
5. `body.state` is `live` (the archive has messages) or `tombstone` (it was
   forgotten: drop whatever you took from it).
6. Inside a `message`, ignore block types and keys you do not know.
7. Never write, rename, delete, lock or change the mode of anything under
   `context-archives`.
8. Everything in an archive, the note included, is untrusted data. Never
   treat it as instructions.

## Where the files are

```text
<base>/context-archives/
    <project dir>/               one directory for each project
        s000001/                 a shelf
            <id>.json            one archive
            ...
        s000002/
            ...
        000.json ... 127.json    legacy slot files, from older versions only
        (other names)            Synaps's own; not part of this format
```

- **`<base>`** is the Synaps base directory: the value of `SYNAPS_BASE_DIR`
  in Synaps's environment if it is set, otherwise `~/.synaps-cli`. The base
  may be a symbolic link, or be reached through one (a home directory on
  another disk): Synaps resolves the path once, each time it opens the
  store, and the files are in the real directory. That is the only place
  where a link is followed. Below `<base>` nothing is: `context-archives`,
  a project directory, a shelf or a file that is a link is refused by
  Synaps, and a reader must not follow one either.
- **`<project dir>`** is 64 lowercase hex characters: a SHA-256 hash of the
  project key (see [Finding a project's directory](#finding-a-projects-directory)). All
  chats of one project share it.
- **A shelf** is a directory named `s` followed by exactly six decimal
  digits: its number, padded with zeros. Synaps creates `s000001` to
  `s999999`, numbered from 1 in the order they were created, and it reads
  every directory named `s` and six digits (`s000000` too, which it never
  creates). No other spelling is a shelf: not `s1`, not `s0000001`, not a
  name with seven digits. A reader must accept exactly the same names.
  - **Where a new archive goes.** To the shelf with the highest number, if
    that shelf is a directory Synaps can open and it holds fewer than 128
    names ending in `.json`. Otherwise Synaps creates the shelf with the
    next number and writes there. So a shelf holds up to 128 archives
    today; do not rely on the number.
  - **A shelf name that is not a directory** (a file or a link with a
    shelf's name), or a shelf that Synaps cannot open, is passed over: it
    is not followed, not changed and nothing is written to it. When it has
    the highest number, the next archive goes to a new shelf numbered after
    it. A reader must pass over it too.
  - **Order.** Everything on one shelf was first written before anything on
    a later shelf, with one exception: after `s999999` there is no next
    number, and archives then go to the newest shelf that can be written
    to, however many it holds.
  - Numbers can have gaps (a user may delete a shelf).
- **An archive** is one file named `<id>.json` in a shelf. `<id>` is the
  archive's id: 32 lowercase hex characters (`0-9`, `a-f`). It is the same
  string as `id` inside the file, as the archive id in hook events, and as
  what follows `ctx-` when the model names an archive.
- **Legacy slot files** `000.json` to `127.json` sit directly in the project
  directory. Versions of Synaps from before shelves wrote them. See
  [Legacy slot files](#legacy-slot-files).
- **Every other name** must be ignored, in the project directory and in a
  shelf: Synaps keeps files of its own there. Their names are not part of
  this format and can change. Do not open them. Today they are:
  - **`lock`**, in the project directory: an empty file that Synaps locks
    while it writes or forgets. See [The lock file](#the-lock-file).
  - **Temporary files**, named `pending.<32 hex characters>.tmp` (older
    versions used the one name `pending.tmp`). Synaps writes every file
    under such a name first, in the directory the file will be in, and
    renames it into place when it is whole. A crash can leave one behind.
    When Synaps next writes an archive to a shelf, it removes the temporary
    files of that shelf that were last written more than one hour ago (plain
    files of its own only; a link or a directory under such a name is
    left); a younger one may belong to a write that is still running. One left in
    the project directory itself, or in a shelf that is no longer written
    to, stays. A temporary file may hold a whole record, part of one, or a
    record that was never stored: never read one.

Synaps creates these directories and files readable by their owner only
(modes 0700 and 0600), so a reader has to run as the same user.

### The lock file

`lock` is not part of the format, and a reader must not open or lock it.
Its rules matter to anyone who backs up or restores these directories:

- Synaps only ever locks it. It never writes to it, truncates it or reads
  it, and the file holds no data.
- It is used as long as it is a regular file owned by the user Synaps runs
  as, **whatever its link count and whatever its mode**. A backup made with
  hard links gives it a second name, and a restore can leave it read-only
  (0400): both are fine. Synaps changes its mode only to take away access
  for group and others.
- It is refused when it is a symbolic link (never followed), not a regular
  file, owned by another user, or closed to its owner by its mode. Nothing
  can then be saved or forgotten in that project, and the error names the
  file and says what is wrong. The fix is to delete the file: Synaps makes
  a new one.
- This holds for `lock` only. An archive file with a second name is still
  refused: see [Reading safely](#reading-safely).

### Finding a project's directory

**The supported way is the hook event.**
[`on_context_archived`](hooks.md#on_context_archived) gives you `data.path`,
the absolute path of the archive file, and `data.project_key`. (The path
starts with `<base>` as Synaps was given it, so it goes through a link if
the base does.) For a file in
a shelf, the project directory is the parent of the file's parent. `path` can
also point at a legacy slot file, when the same content had already been
archived by an older version; then the project directory is the file's
parent. Either way it is the directory whose name equals the `scope` inside
the file. Remember the directory for each project key you see: that is how
you find the same project again when you start.

**From a project key to the directory name** is a fixed rule. Take the key
as UTF-8 bytes, put its length in front as 8 bytes, big-endian, and hash
both with SHA-256. The directory name is the hash in lowercase hex:

```python
import hashlib, struct

def project_dir(project_key: str) -> str:
    key = project_key.encode("utf-8")
    return hashlib.sha256(struct.pack(">Q", len(key)) + key).hexdigest()

project_dir("p0123456789abcdef")
# '01a8f6fb3ec12e1d59d07bd6e3943c84c18415e3111dd5cba218b7ce9218b0c7'
```

This is what you need for
[`on_memory_forgotten`](hooks.md#on_memory_forgotten), which carries a
project key and an id but no path.

**From a chat's folder to the project key: do not rely on working it out
yourself.** Hook events and `tool.call` carry `cwd`, the chat's folder (see
[`cwd`](hooks.md#cwd-the-chats-working-folder)), but the key is derived by
the host, and the result depends on things your process cannot see. For the
record, this is what the host does today with the file store:

1. If `SYNAPS_PROJECT_ROOT` is set in the host's environment, the project
   root is that path.
2. Otherwise it resolves the chat's folder to its canonical path (symlinks
   resolved) and walks upwards, at most 64 levels, to the nearest folder that
   contains an entry named `.git` (a directory or a file) or
   `.synaps-project`. That folder is the project root. If there is none, the
   chat's folder itself is.
3. The key is `p` followed by the first 16 hex characters of the plain
   SHA-256 of the root's canonical path.

So two chats started anywhere inside one Git checkout share a project, and
a Git worktree is a project of its own. Your process may see another
environment and another spelling of the path than the host did, and `cwd` in
an event is not canonical. To learn the key for a folder, take the
`project_key` of an event that came with that `cwd`.

## The record

A file holds one JSON object, in UTF-8. Synaps writes it on a single line
with no spaces; do not rely on the whitespace or on the order of keys.

| Field | Type | Meaning |
|---|---|---|
| `version` | integer | The format version. `1`. Check it before anything else. |
| `scope` | string, 64 hex | The name of the project directory the file is in. |
| `logical` | string, 64 hex | Which chat the archive came from: the chat's session id, hashed the way a project key is hashed into `<project dir>`. Every archive of one chat has the same value, so you can group by it, and if you know a session id (the `session_id` of a hook event) you can hash it and compare. It cannot be turned back into the id. |
| `id` | string, 32 lowercase hex | The archive's id. In a shelf it is the file name without `.json`. |
| `digest` | string, 64 hex | Identifies the content: the chat, the number of source messages and the stored messages. The note is not part of it. Within a project, two records with the same `digest` hold the same messages from the same chat. Treat it as opaque and do not try to recompute it. |
| `created_ms` | integer, 0 or more; **optional** | When the archive was written: milliseconds since 1970-01-01 00:00 UTC, by the clock of the machine that wrote it. A record written before this field existed does not have it, and a tombstone never has it. It is not part of the `digest`: when the same content is sealed again, the file that is there stays as it is, with the time it was first written. |
| `body` | object | What the archive holds. `body.state` says which of the two forms it has. |

`body` when `state` is `live`:

| Field | Type | Meaning |
|---|---|---|
| `state` | string | `live`. |
| `source_message_count` | integer, 0 to 4096 | How many messages of the conversation were sealed into this archive, counting the ones that were not stored. |
| `messages` | array | The stored messages, in conversation order. Can be empty. |
| `note` | string | The chat's working note at the rollover: text the model keeps for itself across windows. At most 8,192 bytes, often empty. Synaps does not search it and does not return it with the messages. It is as untrusted as any message. |

`body` when `state` is `tombstone` has no other field: `{"state": "tombstone"}`.
A tombstone has no `created_ms` either. See [Tombstones](#tombstones).

Each entry of `messages`:

| Field | Type | Meaning |
|---|---|---|
| `source_index` | integer | Position of this message among the sealed messages, counting from 0. Always below `source_message_count`, and strictly increasing through the array. A gap means a message in between was not stored. |
| `block_indices` | array of integers | Which content blocks of the original message were stored, by position, in increasing order. Empty when `content` is a string. A gap means a block in between was not stored. |
| `message` | object | The message. |

These fields are all there is at the record level. Version 1 has no other
field in the record, in `body`, or in an entry of `messages`, and
`created_ms` is the only one that a record may lack.

### The message

| Key | Type | Meaning |
|---|---|---|
| `role` | string | `user` or `assistant`. |
| `content` | string, or array of blocks | The text of the message, or its content blocks. |
| `_archive_reduced` | string, usually absent | Present when the message was over a limit by itself and is not stored exactly as it was. See [Reduced messages](#reduced-messages). |

A block is an object with a `type`:

| `type` | Other keys | Where |
|---|---|---|
| `text` | `text`: string | Either role. |
| `tool_use` | `id`: string; `name`: string, the tool's name; `input`: the tool's arguments, any JSON value | `assistant` messages. |
| `tool_result` | `tool_use_id`: string, the `id` of the call it answers; `content`: a string, or an array of `text` blocks; `is_error`: boolean, present only if the original had it | `user` messages. |

`message` is the one place that may grow without a new version: see
[What `version` means](#what-version-means). Ignore a block whose `type` you
do not know, and any key you do not know.

### What is not stored

An archive is not a transcript. Synaps leaves out:

- system and developer messages, and any message that is neither `user` nor
  `assistant`;
- reasoning ("thinking") blocks, images, documents, and every block type
  other than the three above;
- messages and blocks the host has marked private, restricted or not to be
  kept;
- the host's own notes in the conversation, such as the continuation message
  a rollover leaves behind;
- every key of a message other than `role` and `content`, and every key of
  a block other than the ones in the table above;
- a tool call or result whose id, or a call whose tool name, is not a plain
  identifier (letters, digits and `_ - . :`, at most 128 characters);
- a message that has nothing left to store. That is why `source_index` can
  skip numbers.

What is stored is the original after screening: Synaps does not shorten,
summarise or reformat it. There are two exceptions, both marked in the file:
content that was withheld, and a [reduced message](#reduced-messages).

### Withheld content

Synaps screens every string before storing it. Something it will not store
is replaced whole, never in part:

- a string becomes the placeholder `[archive: withheld]`. This happens to
  text that reads like a credential, and to a tool result that has no text
  to store (only an image, say) or that the host has no way to screen. The
  `note` can be this placeholder too;
- a tool's `input` becomes an object with the one key `_archive_withheld`:
  `{"_archive_withheld": true}`. The input of a `context_checkpoint` call is
  always withheld.

Both are ordinary values, and a conversation can contain the same words. The
built-in screen is a short list of words such as "password", on top of
whatever the host redacts. It is not a secret detector. Assume an archive can
hold anything the conversation held.

### Sizes

Version 1 files are at most 16 MiB, hold at most 4,096 messages, and have a
note of at most 8,192 bytes. Synaps does not write larger ones and does not
read them.

## What `version` means

- **Check `version` first.** If it is a number you do not know, skip the
  file without failing and read the rest. Do not guess at a newer format.
- **Version 1 is exactly the record-level fields above.** One of them,
  `created_ms`, was added before version 1 was released, as an optional
  field: a record written before that does not have it and is as valid as
  one that does. From this definition on, while it writes `"version": 1`,
  Synaps will not add, remove or rename a field of the record, of `body`,
  or of an entry of `messages`, and will not change what one means. A change
  of that kind comes with a higher `version`. Synaps's own reader holds
  itself to this: it rejects a version 1 file with any other record-level
  field, with a `created_ms` that is not a whole number of 0 or more
  (`null` included), or with a `created_ms` in a tombstone.
- **The contents of `message` may grow** without a new version: a new block
  `type`, a new key in a block or in the message, a new marker in a
  [reduced message](#reduced-messages) (`_archive_json` was added this way).
  A reader must ignore what it does not know.
- **The layout is part of the version.** The directory and file names on
  this page belong to version 1.
- There is no version 2 today, and nothing in Synaps rewrites a file from
  one version to another.

## What you can rely on

While Synaps writes version 1:

1. **A file with a final name is complete.** Synaps writes each file under a
   temporary name and renames it into place only when it is whole and on
   disk. You never see half a record under a name you are told to read.
2. **A live archive never changes.** Once written, its bytes stay as they
   are.
3. **The only change to a file is live to tombstone,** once, under the same
   name, by the same kind of rename. You read the whole live record or the
   whole tombstone, never a mix. A tombstone keeps `version`, `scope`,
   `logical`, `id` and `digest`, and nothing else: not `created_ms`. (A
   forget can also add one new file, itself a tombstone: see
   [Tombstones](#tombstones).)
4. **Synaps never deletes an archive file,** never moves one, and never
   merges, renumbers or removes shelves. Sealing an archive never touches
   another archive.
5. **Names:** `<project dir>` is the hash of the project key; an archive in
   a shelf is in a file named by its `id`; `scope` is the name of the
   directory the file is in.
6. **Content that Synaps finds stored is not stored again.** Sealing the
   same messages from the same chat again writes nothing and gives the `id`
   of the archive that is there. A different note does not make a new
   archive and does not change the one that is there. This is not a promise
   that a `digest` is in one file only: where Synaps cannot see the first
   copy (a shelf it cannot open, a file it will not read, a legacy slot
   file it cannot read), it stores the content again, and a project can
   hold the same `id` on two shelves (a restored backup). An archive brought
   in by an import ([Moving archives](#moving-archives-between-machines-or-projects))
   keeps the id it had, so its content can afterwards exist under a second
   id. See [Tombstones](#tombstones) for what that means for a forget.
7. **No lock is needed to read.** Synaps reads archives in shelves without
   one.

Do not rely on:

- file modification times, or the order in which a directory lists its
  entries;
- a record having `created_ms`, its being close to the file's own times, or
  its growing from one archive to the next: it is one machine's clock at
  one moment, and a clock can be wrong or be set back;
- how many archives a shelf holds;
- any relation between `id` and `digest`. Today a new archive's `id` is the
  first 32 characters of its `digest`, or the last 32 when another file
  already has the first name; the id of a legacy archive is random;
- an `id` being in one file only. The same `<id>.json` can be on two
  shelves. When Synaps is asked for an id, it takes the valid record on the
  newest shelf that has one, and passes over a file of that name that is
  not a valid record;
- whitespace, key order or a trailing newline in the JSON;
- the names or the presence of the other files in these directories;
- one context window being one archive (see [Parts](#parts));
- having been told about every archive, or only once (see
  [When to look](#when-to-look)).

## Reading safely

You must tolerate:

- **A tombstone,** the first time you read a file or any time after.
- **Files and shelves that appear while you list.** A listing is a snapshot.
  Read what you listed; pick up the rest on the next scan or from the hook.
- **A file that changes between your listing and your read.** It can become
  a tombstone in that time. You get one whole record either way.
- **A file or a shelf that is gone.** Synaps does not delete them, but a
  user can.
- **A file you cannot use:** an unknown `version`, JSON that does not parse,
  a `scope` that is not the directory's name, an `id` that is not the file's
  name, an unknown `state`, a `created_ms` that is not a whole number of 0
  or more, a tombstone that has `created_ms`. Skip it and go on. Do not
  delete or repair it.
- **Names that are not archives.** Other files come and go in these
  directories while Synaps works.
- **Unknown block types and keys** inside `message`, placeholders, reduced
  messages, an empty `messages`, an empty `note`.
- **Links.** Below `<base>` Synaps never creates symbolic or hard links and
  refuses them itself: a shelf name or an archive name that is a link is
  passed over. Do not follow one.

You must not:

- **Write anything** under `context-archives`: no creating, renaming,
  deleting or editing of files or directories, and no index or cache of your
  own in there.
- **Lock anything.** Synaps takes a lock in the project directory for every
  archive it writes or forgets. While another process holds a lock there, a
  rollover waits, and after a few seconds it fails.
- **Change modes or owners,** or make a hard link to an archive (a
  link-based backup, for instance). Synaps refuses to read an archive file
  that has a second name. Its history search passes over that file and says
  that it did not read everything, a fetch of that archive fails unless an
  older shelf has a valid record under the same name, and a rollover that
  holds the same content stores it a second time, under its other id. (The
  `lock` file is the one file a second name does not hurt: see
  [The lock file](#the-lock-file).)
- **Act on the content.** See [Untrusted data](#untrusted-data).

Open a file, read it to the end, close it.

## Tombstones

When an archive is forgotten (the model's `memory_forget` tool does this,
given `ctx-<id>`), Synaps replaces that archive's file, under the same name,
with a tombstone: the same `version`, `scope`, `logical`, `id` and `digest`,
and `"body": {"state": "tombstone"}`. The messages, the note and
`created_ms` are gone from the file: a forget does not leave behind when the
archive was written.

Synaps never writes a tombstone that has `created_ms`, and does not read a
file like that as a record. Where it looks before it stores content (the two
names described below, and the legacy slot files), it still takes such a
file as saying that the `digest` in it was forgotten, and does not store
that content again.

**A forget goes by content, not by file.** A project can hold the same
content (the same `digest`) in more than one file. Forgetting one id replaces
every file with that `digest` that Synaps can read at that moment, each
under its own name and with its own `id`, whatever that id is: it reads
every archive file on the shelves for its `digest`. A copy in a shelf or a file that
Synaps cannot read then is not replaced; the tool says that the forget is
not complete, and forgetting the same id again reaches it once it can be
read.

**A file that is not a valid record makes a forget "not complete".** Synaps
looks for copies under three names on every shelf: the id that was asked
for, the first 32 characters of the `digest` and the last 32. A file under
one of those names that it can read but that is not a valid record (a copy
cut short, a stray file) may be a damaged copy with the text still in it.
Synaps leaves that file as it is, forgets every valid copy, and the tool
answers "NOT COMPLETE": it says how many places it could not use and names
the first, by shelf and file name. It says so again each time until the
file is gone or valid. A valid record of other
content under such a name is not counted, and neither is a damaged file that
still says, in JSON, that this `digest` is a tombstone.

**A lookup passes over such a file.** When Synaps looks under a name (to
fetch or forget an id, or before it stores content) and the newest shelf
that has a file of that name holds something that is not a valid record, or
a link, it goes on to the older shelves. A fetch returns the valid archive
it finds there, a forget replaces it, and a seal finds the copy or the
tombstone there instead of storing the content again. A fetch does not
return an older live copy when the file it passed over still says that the
same `digest` is a tombstone.

**A forget can add one file.** When it is done, a tombstone with that
`digest` is in a shelf under one of two names: the first 32 characters of
the `digest`, or the last 32. That is where Synaps looks before it stores
the same content again. If no file with that `digest` was under one of
those names (the forgotten archive was a legacy slot file, whose id is
random), Synaps writes that tombstone as a new file in the newest shelf,
before it replaces anything. Forgetting an archive whose id is not derived
from its digest (one that was brought in from another store and kept the id
it had) also leaves a tombstone under the digest's own id, in the same way. Its `id` is the file's name, and no archive was
ever announced with it. It uses the first of the two names that no shelf
has a file under; when other files have both, it writes none, and Synaps
then has no name to store that content under either.

- **If any file with a `digest` is a tombstone, that content is forgotten,**
  whatever another file with the same `digest` says. Drop what you indexed
  from every one of them.
- **Drop what you indexed from it.** A file can already be a tombstone the
  first time you see it.
- **A tombstone stays.** Synaps does not delete it or bring the archive
  back, and it refuses to store the same messages from the same chat again.
  Before it stores content it looks under both names on every shelf it can
  open, so a tombstone on an older shelf counts whatever a newer shelf has
  under that name. (Two exceptions. A tombstone in a shelf that Synaps
  cannot open is not seen. And a legacy slot file that was forgotten before
  a forget added the second tombstone has only itself to say so: if Synaps
  can no longer read that file, it no longer blocks that.)
- **It forgets one archive, not a text.** The same words can be in another
  archive that holds different messages around them. And replacing a file
  does not erase backups or filesystem snapshots, or what you copied.
- [`on_memory_forgotten`](hooks.md#on_memory_forgotten) tells you when it
  happens, if you are running: once for the id that was asked for, and once
  for each other id whose file the forget replaced. The tombstone is the
  record.

## Parts

One archive has limits: 4,096 messages and 16 MiB, among others. The store
can save a context window that is over a limit as several archives, called
**parts**: the window is cut into consecutive runs of messages, and each run
becomes an ordinary archive.

**The files record nothing about parts.** No field says that an archive is a
part, which window it came from, how many parts there are, or where in the
window it starts. The store hands that list (the parts in order, and where in
the window each one starts) to the host when it seals the window, and only
the host has it. It reaches an extension through the
[`on_context_archived`](hooks.md#on_context_archived) event, which says which
part of how many its archive is (`part`, `parts`); hooks.md has the fields.
The chat that rolled over also records, in its own session history, which
archive its new window continues from; that history is not an archive file
and not part of this format. If you need the order of parts, take it from the
events and keep it yourself.

What a part looks like:

- Its `source_message_count` and its `source_index` values count from the
  part's own first message, not from the start of the window.
- The window's `note` is stored with the last part. The other parts have an
  empty note.
- All parts have the same `logical`.
- Two parts with identical content are one archive: one file, one `id`. So
  are a part and an archive that was stored earlier. An archive that was
  already there keeps the note it was first stored with.
- A [reduced message](#reduced-messages) is always a part by itself.

Where a window is cut is not part of the format. Today a part is filled from
where the one before ended until the next message would take it over a
limit, so the parts of a window are as few as they can be, apart from the
two more that each reduced message costs.

**The same messages can be in two archives.** A rollover that fails after
some parts are written (a full disk, a cancelled turn) leaves those parts
where they are, and announces nothing. The next attempt finds them and
writes only what is missing. If the chat got longer in between, the last
part of the earlier attempt no longer ends the window: the new attempt
stores those messages again, in a longer last part, and the earlier one
stays as an archive that no window refers to.

With the files alone you **can** read every archived message (each archive
is complete in itself and none needs another), keep the order of messages
inside one archive, group archives by chat with `logical`, and tell a
reduced message from a whole one.

With the files alone you **cannot** tell that two archives are parts of one
window, put parts or windows in order for certain (a record has no sequence
number, and an `id` says nothing about order; `created_ms`, where a record
has it, is a clock reading, and a part whose content was already stored
keeps the time of that earlier write; the shelf number only tells you that
one shelf is older than the next), find where in its window a part starts,
or always find a window's note: when its last part is content that was
already stored, no file holds it.

## Reduced messages

A single message can be over a limit of an archive all by itself, so that no
cut between messages helps. Synaps then stores it **reduced**: changed by as
little as makes it fit, with a marker in the place of everything it changed.
A reduced message is alone in its archive.

A reduced message has the key `_archive_reduced` beside `role` and
`content`. Its value is one of three sentences, where each N is a count of
markers in the message:

- `this message alone is over the per-archive limit; it is stored with N omission(s), each marked in place`
- `this message alone is over the per-archive limit; it is stored with N tool input(s) as JSON text, each marked in place`
- `this message alone is over the per-archive limit; it is stored with N omission(s) and N tool input(s) as JSON text, each marked in place`

**Test for the key.** Only Synaps sets it: a key of that name in the
original message is not copied, so a message that has it is reduced and a
message without it is stored exactly. The markers inside the content are
ordinary values, and a conversation can contain the same words and the same
keys: they are Synaps's markers only in a message that has
`_archive_reduced`.

Synaps changes as little as it can, in four steps, and takes a step only
when the one before was not enough. The first two leave nothing out when the
trouble is only the shape of a tool input:

1. **Tool inputs that are over a limit by themselves.** An archive limits
   how deeply a tool input may be nested (29 levels) and how many values it
   may hold. An input over one of those limits is stored as one string: the
   `input` becomes an object with the one key `_archive_json`, whose value
   is the whole input as JSON text, screened like every other string.
   Nothing of it is missing. Parsing the string gives the input back, unless
   the screen changed the text, so be ready for a string that does not
   parse. An input that is too large as text as well is left out: the
   `input` becomes an object with the one key `_archive_omitted`, whose
   value is the marker
   `[archive: tool input of N bytes omitted, over the per-archive limit]`,
   where N is the size of the input as JSON. Everything else stays exact.
2. **Every tool input** of the message as JSON text, under `_archive_json`
   in the same way: for inputs that each fit and together hold too many
   values. One that is too large as text by itself is left out, with the
   `_archive_omitted` marker.
3. **Every tool input** left out, each with the `_archive_omitted` marker.
4. **Every tool input left out, long strings and long block lists cut:**
   - a string longer than 4,096 bytes is cut to its first 4,096 bytes (a few
     less if a character would be split), followed by a line break and
     `[archive: N bytes omitted, over the per-archive limit]`, where N is the
     number of bytes cut off;
   - of the content blocks, the first 256 that are stored stay. One `text`
     block takes the place of all the rest:
     `[archive: N blocks omitted, over the per-archive limit]`. N counts the
     blocks of the original message from that position on, stored or not.
     In `block_indices` this block has the position of the first block it
     stands for;
   - a tool result whose `content` was an array of blocks becomes one
     string: its first text, cut like any string, then, if more blocks
     followed, a line break and the same marker, where N counts the blocks
     after that text.

The numbers 29, 4,096 and 256 are today's; read the markers rather than
count. An input that is withheld (see [Withheld content](#withheld-content))
stays withheld in every step: it is `{"_archive_withheld": true}`, never
text.

This is the message of the reduced fixture. Its text was a 16 MiB build log
and is shortened here once more, at the `...`; the file has all 4,096 bytes:

```text
{
  "_archive_reduced": "this message alone is over the per-archive limit; it is stored with 2 omission(s), each marked in place",
  "content": [
    {"text": "build log line 0000001\nbuild log line 0000002\n...\nbu\n[archive: 16773139 bytes omitted, over the per-archive limit]", "type": "text"},
    {"id": "toolu_04", "input": {"_archive_omitted": "[archive: tool input of 48 bytes omitted, over the per-archive limit]"}, "name": "write", "type": "tool_use"}
  ],
  "role": "assistant"
}
```

And this is the message of the fixture with a tool input stored as JSON
text. The tool was called with twenty groups nested one inside the other,
which is forty levels of JSON. The string is shortened here at the `...`;
the file has all 463 bytes of it. The second input is stored as it was:

```text
{
  "_archive_reduced": "this message alone is over the per-archive limit; it is stored with 1 tool input(s) as JSON text, each marked in place",
  "content": [
    {"text": "Evaluating the nested expression.", "type": "text"},
    {"id": "toolu_08", "input": {"_archive_json": "{\"of\":[{\"of\":[{\"of\":[ ... {\"op\":\"leaf\",\"value\":1} ... ],\"op\":\"group\"}],\"op\":\"group\"}"}, "name": "evaluate", "type": "tool_use"},
    {"id": "toolu_09", "input": {"path": "src/eval.rs"}, "name": "read", "type": "tool_use"}
  ],
  "role": "assistant"
}
```

## Legacy slot files

Before shelves, Synaps kept at most 128 archives for a project, in files
named `000.json` to `127.json` directly in the project directory. Those
files are still read where they are. Synaps tombstones them in place, adds
no new ones, and does not move them to shelves.

A slot file holds the same record as any other archive, with one difference:
**the file name is a slot number, not the id.** Take the `id` from inside
the record. (It is a random id, unrelated to the `digest`.) To find a legacy
archive by id you have to read the slot files.

## When to look

Two hooks tell an extension that something changed. Both are described in
[hooks.md](hooks.md):

- [`on_context_archived`](hooks.md#on_context_archived) is sent after an
  archive file has been written. It carries the archive's id, the path of
  its file, the project key, the message counts, and `part` and `parts`.
- [`on_memory_forgotten`](hooks.md#on_memory_forgotten) is sent after
  `memory_forget` has forgotten an archive or a note. It carries the id and
  the project key.

Both are [notify-only](hooks.md#notify-only-hooks): Synaps does not wait for
you, an event can be lost, and one can arrive twice or out of order. hooks.md
has the exact payloads and what each event does and does not promise.

**The files are the truth and the hook is a hint.** If your index has to be
complete, scan when you start: for each project directory you know, read it
as [In short](#in-short) says, add the ids you do not have, and drop the ones
that have become tombstones. Key everything on the archive `id`.

## Moving archives between machines or projects

Do not copy archive files into a project directory yourself: `scope` has to
be the name of the directory, and a file dropped into a shelf is checked by
nobody. `synaps memory import-archives` is the way in. It is a command for
the person who owns the data, not something an extension calls for itself;
it is described here because its input is the record of this page.

```text
synaps memory import-archives <FILE>... (--as-recorded | --cwd <DIR> | --project-key <KEY>) [--dry-run]
synaps memory project [--cwd <DIR>]
```

**The input is JSON Lines: one record per line,** the same object an archive
file holds ([The record](#the-record)), live or tombstone. `-` is standard
input; any other input must be a regular file (not a directory, a FIFO or a
device), and every input is opened before a line is imported. Blank lines
are skipped, and lines are imported in the order given,
across the files in the order they are named. A directory of archive files
is the same thing, one file after another with a newline between them:

```sh
for f in "$dir"/*.json "$dir"/s*/*.json; do cat "$f"; echo; done | synaps memory import-archives - --as-recorded
```

**There is no default destination.** One of the three must be given:

- `--as-recorded` files each record in the project directory its own
  `scope` names, and changes nothing in it;
- `--cwd DIR` files every record under the project of that folder, found as
  for a chat started there (refused while `SYNAPS_PROJECT_ROOT` is set:
  that variable makes every folder one project);
- `--project-key KEY` files every record under that project key.

With the last two, `scope` is set to the new directory's name. That is the
only field an import changes: `scope` is not part of the `digest`.
`synaps memory project` prints the project key, the archive directory and
the notes file of a folder, and creates nothing.

What an import does with a record:

- It is stored **under the id it has**, with its own `logical`, `digest` and
  `created_ms`, as a new file in the newest shelf. Nothing is recomputed to
  make it fit. So `id` need not be derived from `digest` (you were already
  told not to rely on that), and the same content can be in the project
  under a second id.
- It is refused, and nothing is written for it, when Synaps would not read
  it as a record of this page, when its `digest` is not the digest of its
  content, when a `message` is not a JSON object, when a tombstone has
  `created_ms`, when its id is already there with other content, when its
  content was forgotten in this project, or when it is a tombstone of
  content that is live in this project under another id (forget that
  archive here, then import again).
- The same record a second time changes nothing.
- A tombstone that comes in for an archive that is live here forgets it, as
  [Tombstones](#tombstones) describes. A live record that comes in for an
  archive that is a tombstone here is not stored: **an import does not undo
  a forget.**
- No file is replaced, except by that forget.

The command prints one JSON document: the totals (`read`, `stored`,
`already_present`, `forgotten`, `kept_forgotten`, `refused`), then one entry
for each line with its id, its outcome and, when it was refused, the name of
the check that failed. Nothing in the document is content. A tombstone
whose forget may not have reached every copy (see [Tombstones](#tombstones))
has `unread` in its entry and its id in `forgets_not_complete`. The exit
code is 0 when nothing was refused, 1 when something was or when a forget is
in `forgets_not_complete`, and 2 when the run
stopped (an input that cannot be read, a store that cannot be read or
locked): what was imported before the stop is complete, and running the
same command again continues. When a store stops at a record, `stopped`
names the line, the record's id and the project directory: a file under
that id that is not a record (`<id>.json` on one of its shelves) stops
every run there until it is moved away. `--dry-run` writes nothing at all and prints
what the real run would print.

What to know before you rely on it:

- **No hook is sent for an imported archive.** The command is not a chat:
  `on_context_archived` and `on_memory_forgotten` do not fire for what it
  stores or forgets. An index finds imported archives and their tombstones
  the way [When to look](#when-to-look) says: by scanning.
- **It writes the built-in file store whatever `memory.backend` says,** and
  never talks to another backend.
- **It may run while chats are running.** It takes the same lock a seal
  takes, one record at a time.
- **A record that names another project is moved, not shared.** Chats find
  an archive by project: after `--cwd` or `--project-key`, only chats of
  that project can fetch it, under the same `ctx-<id>`.
- **The base directory must be there.** It is `SYNAPS_BASE_DIR`, or
  `~/.synaps-cli`, and the document prints it as `base`. The command stops
  (exit code 2) when it is not an absolute path, and a real run stops when
  it does not exist: an import does not create a base.
- **A refused record leaves no directory behind:** a project directory is
  made when the first record for it is stored.
- **The longest line is 16 MiB and 4 KiB.** A longer one is refused without
  being read into memory, and the run goes on.

## Untrusted data

An archive holds what people and models wrote and what tools returned. The
`note` is text a model wrote for itself. None of it is an instruction to
your extension or to the model you may show it to: quote it as data, mark
where it came from, and never do what it says because it says so.

The files hold conversation text. It has been screened, which is not a
promise that it holds no secrets. Extension permissions decide which events
Synaps sends you; they do not guard the files, which any process running as
the same user can read.

## A worked example

The fixtures in [`fixtures/context-archives/`](fixtures/context-archives/)
are one project directory: project key `p0123456789abcdef`, so the directory
is `01a8f6fb3ec12e1d59d07bd6e3943c84c18415e3111dd5cba218b7ce9218b0c7`. It
holds one chat, session id `20260501-120000-abcd`, whose hash is the
`logical` of every record:
`900c08a5903292975711f75ce85a7b6e30fab1b1223378ab1111cdbb7bc3d809`. The
files are real: Synaps wrote them, `000.json` with a version from before
shelves. One thing in them was set by hand afterwards: each `created_ms` is
a time on the day of the made-up session, not the time the file was made.
Nothing in them is from a real conversation.

```text
01a8f6fb3ec12e1d59d07bd6e3943c84c18415e3111dd5cba218b7ce9218b0c7/
    000.json                                         a legacy slot file
    s000001/
        3fe5a51ef0d405efec79538b7717e255.json        holds a reduced message
        5ad68965a84888ab221823f5b4490804.json        holds a tool input as JSON text
        693c14d38641fb504a642f0e7cbfaac9.json        a tombstone
        b436b6196e6ca0758c70d5faed628f29.json        a live archive
```

**A live archive.** Say an `on_context_archived` event arrives with
`data.path` ending in `s000001/b436b6196e6ca0758c70d5faed628f29.json`. This
is that file, with line breaks added (the file itself is one line):

```json
{
  "version": 1,
  "scope": "01a8f6fb3ec12e1d59d07bd6e3943c84c18415e3111dd5cba218b7ce9218b0c7",
  "logical": "900c08a5903292975711f75ce85a7b6e30fab1b1223378ab1111cdbb7bc3d809",
  "id": "b436b6196e6ca0758c70d5faed628f29",
  "digest": "b436b6196e6ca0758c70d5faed628f297d393b0e2248dcceb082fd75e087653e",
  "created_ms": 1777639267512,
  "body": {
    "state": "live",
    "source_message_count": 7,
    "messages": [
      {"source_index": 1, "block_indices": [],
       "message": {"content": "Where is the retry limit set?", "role": "user"}},
      {"source_index": 2, "block_indices": [1, 2, 3],
       "message": {"content": [
         {"text": "I will look in the network module.", "type": "text"},
         {"id": "toolu_01", "input": {"path": "src/net/retries.rs"}, "name": "read", "type": "tool_use"},
         {"id": "toolu_02", "input": {"path": "src/net/retry.rs"}, "name": "read", "type": "tool_use"}
       ], "role": "assistant"}},
      {"source_index": 3, "block_indices": [0, 1],
       "message": {"content": [
         {"content": "No such file: src/net/retries.rs", "is_error": true, "tool_use_id": "toolu_01", "type": "tool_result"},
         {"content": [{"text": "pub const MAX_RETRIES: u32 = 5;\n", "type": "text"}], "tool_use_id": "toolu_02", "type": "tool_result"}
       ], "role": "user"}},
      {"source_index": 4, "block_indices": [0, 1],
       "message": {"content": [
         {"text": "MAX_RETRIES is 5, in src/net/retry.rs.", "type": "text"},
         {"id": "toolu_03", "input": {"_archive_withheld": true}, "name": "context_checkpoint", "type": "tool_use"}
       ], "role": "assistant"}},
      {"source_index": 5, "block_indices": [0],
       "message": {"content": [
         {"content": "ok", "tool_use_id": "toolu_03", "type": "tool_result"}
       ], "role": "user"}},
      {"source_index": 6, "block_indices": [],
       "message": {"content": "[archive: withheld]", "role": "user"}}
    ],
    "note": "Goal: find where the retry limit is set. Done: MAX_RETRIES is 5, in src/net/retry.rs. Next: nothing."
  }
}
```

Reading it:

- `version` is `1`, `scope` is the directory's name and `id` is the file's
  name, so this is a record you can use. `state` is `live`.
- `created_ms` is 1777639267512: written on 1 May 2026 at 12:41:07 UTC.
- `source_message_count` is 7 and there are 6 messages. `source_index` 0 is
  missing: it was a system message.
- The second message has `block_indices` `[1, 2, 3]`: block 0 of the
  original was a reasoning block. Its two `tool_use` blocks are answered by
  the two `tool_result` blocks of the next message, matched by `id` and
  `tool_use_id`. The first result has `is_error`; the second has its
  `content` as an array of `text` blocks.
- The `context_checkpoint` call has its `input` withheld, and the last
  message is `[archive: withheld]`: it contained the word "password".
- The `note` is what the model wanted to remember. It is data.

**A tombstone.** The archive `693c14d38641fb504a642f0e7cbfaac9` was
forgotten. Its file is still there under the same name, without the
messages, the note and the `created_ms` it had:

```json
{
  "version": 1,
  "scope": "01a8f6fb3ec12e1d59d07bd6e3943c84c18415e3111dd5cba218b7ce9218b0c7",
  "logical": "900c08a5903292975711f75ce85a7b6e30fab1b1223378ab1111cdbb7bc3d809",
  "id": "693c14d38641fb504a642f0e7cbfaac9",
  "digest": "693c14d38641fb504a642f0e7cbfaac9336ef9061b19ba90ca4a6320b4e698f2",
  "body": {"state": "tombstone"}
}
```

**A legacy slot file.** `000.json` was written by a version from before
shelves. The record is the same; the `id` is not the file name and is not
the start of the `digest`, and there is no `created_ms`, as in every record
from before that field:

```json
{
  "version": 1,
  "scope": "01a8f6fb3ec12e1d59d07bd6e3943c84c18415e3111dd5cba218b7ce9218b0c7",
  "logical": "900c08a5903292975711f75ce85a7b6e30fab1b1223378ab1111cdbb7bc3d809",
  "id": "4c459668c1384294b09674c915b29c2d",
  "digest": "beafe0324c71b66abd1627ecbd410c72a967e077f2a1e340d3cd7dbee9e9ab17",
  "body": {
    "state": "live",
    "source_message_count": 2,
    "messages": [
      {"source_index": 0, "block_indices": [],
       "message": {"content": "What does `make check` run?", "role": "user"}},
      {"source_index": 1, "block_indices": [],
       "message": {"content": "It runs cargo fmt --check, then cargo clippy, then the tests.", "role": "assistant"}}
    ],
    "note": "Answered a question about the Makefile. Nothing pending."
  }
}
```

**A reduced message** is in `3fe5a51ef0d405efec79538b7717e255.json`, and
**a tool input stored as JSON text** in
`5ad68965a84888ab221823f5b4490804.json`; both are shown
[above](#reduced-messages).

## A reference reader

[`examples/extensions/context_archive_reader.py`](../../examples/extensions/context_archive_reader.py)
lists one project directory by the rules of this page and passes over
everything else. It is tested against the fixtures and against a directory
full of things a reader must skip.

```text
$ python3 examples/extensions/context_archive_reader.py \
    docs/extensions/fixtures/context-archives/01a8f6fb3ec12e1d59d07bd6e3943c84c18415e3111dd5cba218b7ce9218b0c7
live       4c459668c1384294b09674c915b29c2d  000.json                                       2  2  0  0  -
live       3fe5a51ef0d405efec79538b7717e255  s000001/3fe5a51ef0d405efec79538b7717e255.json  1  1  1  0  1777640575090
live       5ad68965a84888ab221823f5b4490804  s000001/5ad68965a84888ab221823f5b4490804.json  1  1  1  1  1777642219347
tombstone  693c14d38641fb504a642f0e7cbfaac9  s000001/693c14d38641fb504a642f0e7cbfaac9.json
live       b436b6196e6ca0758c70d5faed628f29  s000001/b436b6196e6ca0758c70d5faed628f29.json  6  7  0  0  1777639267512
```

The columns are separated by tabs (shown aligned here): the state, the id,
the file, and for a live archive the number of messages stored,
`source_message_count`, the number of reduced messages, the number of tool
inputs stored as JSON text, and `created_ms` (`-` when the record has
none). Files it skips are named on standard error with the reason.

```python
#!/usr/bin/env python3
"""Reference reader for Synaps context archives, format version 1.

    context_archive_reader.py <project directory>

Lists every archive in one project directory and passes over everything
else, as docs/extensions/context-archives.md tells a reader to. It only
reads: it never writes, locks, renames or deletes. Standard library only.
"""
import json
import os
import re
import stat
import sys

SHELF = re.compile(r"s[0-9]{6}")
ARCHIVE = re.compile(r"[0-9a-f]{32}\.json")
SLOT = re.compile(r"(0[0-9][0-9]|1[01][0-9]|12[0-7])\.json")
MAX_BYTES = 16 * 1024 * 1024


def listing(directory):
    """The names in a directory, sorted. A link, a file or nothing has none."""
    try:
        if stat.S_ISDIR(os.lstat(directory).st_mode):
            return sorted(os.listdir(directory))
    except OSError:
        pass
    return []


def read(path, scope, name):
    """The version 1 record in the file `path`. Raises when it is not one."""
    info = os.lstat(path)
    if not stat.S_ISREG(info.st_mode) or info.st_size > MAX_BYTES:
        raise ValueError("not a regular file of at most 16 MiB")
    with open(path, "rb") as file:  # one read, then closed
        record = json.loads(file.read().decode("utf-8"))
    if not isinstance(record, dict) or type(record.get("version")) is not int:
        raise ValueError("not a record")
    if record["version"] != 1:
        raise ValueError("version %d is newer than this reader" % record["version"])
    if record["scope"] != scope:
        raise ValueError("a record of another project")
    file_name = "%s.json" % record["id"]
    if not ARCHIVE.fullmatch(file_name) or name not in (None, file_name):
        raise ValueError("the id is not 32 hex characters, or not the file's name")
    if record["body"]["state"] not in ("live", "tombstone"):
        raise ValueError("unknown state")
    if "created_ms" in record:  # optional, and never in a tombstone
        created = record["created_ms"]
        if type(created) is not int or not 0 <= created < 2**64:
            raise ValueError("created_ms is not a whole number of milliseconds")
        if record["body"]["state"] == "tombstone":
            raise ValueError("a tombstone with created_ms")
    return record


def inputs_as_text(message):
    """The tool inputs of a message that are stored as one string, their JSON text."""
    if "_archive_reduced" not in message or not isinstance(message["content"], list):
        return []  # only Synaps sets that key; without it an input is the tool's own
    blocks = [block for block in message["content"] if isinstance(block, dict)]
    inputs = [block.get("input") for block in blocks if block.get("type") == "tool_use"]
    texts = [value.get("_archive_json") for value in inputs if isinstance(value, dict) and len(value) == 1]
    return [text for text in texts if isinstance(text, str)]  # json.loads(text) is the input


def describe(record, relative):
    body = record["body"]
    if body["state"] == "tombstone":
        return "tombstone", record["id"], relative
    messages = [entry["message"] for entry in body["messages"]]
    reduced = sum("_archive_reduced" in message for message in messages)
    as_text = sum(len(inputs_as_text(message)) for message in messages)
    counts = len(messages), body["source_message_count"], reduced, as_text
    return ("live", record["id"], relative, *counts, record.get("created_ms", "-"))


def main(project):
    project = os.path.realpath(project)
    scope = os.path.basename(project)
    # Legacy slot files, named by number; then the shelves, each file named by its id.
    files = [(name, None) for name in listing(project) if SLOT.fullmatch(name)]
    shelves = [name for name in listing(project) if SHELF.fullmatch(name)]
    for shelf in sorted(shelves, key=lambda name: int(name[1:])):
        names = listing(os.path.join(project, shelf))
        files += [(shelf + "/" + name, name) for name in names if ARCHIVE.fullmatch(name)]
    for relative, name in files:
        path = os.path.join(project, *relative.split("/"))
        try:
            print(*describe(read(path, scope, name), relative), sep="\t")
        except (OSError, ValueError, KeyError, TypeError) as error:
            reason = "%s: %s" % (type(error).__name__, error)
            print("skipped", relative, reason, sep="\t", file=sys.stderr)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
```
