# Turn origin

`Welcome.features` advertises `turn_origin`, and `turn_origin_owner` for the
owner claim (see [The owner claim](#the-owner-claim)).

A turn origin tells the model which client wrote a user turn. A chat client
that speaks for the owner (a desktop app with a composer, say) creates an
owned session, or proves with an owner claim that its connection is the
owner app's own, declares a short label in its `hello` and marks the turns
the owner types in its own input box with that label. Text it marks reaches the
model inside a synaps-turn wrapper whose origin attribute is that label.
Everything else (other clients, tools, events, workers, summaries, the model's
own text) reaches the model unwrapped. Before any wrapper is added, every text
the model reads is neutralized, so only SynapsCLI writes the wrapper's pair of
tags and the host's own notice headers.

## The label: `client.turn_origin`

A client opts in by declaring its label in `hello`, inside `client`:

```json
{"type":"hello","protocol_version":3,"client":{"kind":"chat","instance":"…","turn_origin":"my-app"},"cwd":"/work","client_version":"…"}
```

(abridged: the other `hello` fields are unchanged.)

- 1 to 32 characters of `a-z`, `0-9` and `-`. The client checks it before it
  connects; the daemon checks it again and refuses the handshake
  (`refused`, reason `protocol`) for any other value. The label is written
  into the model's wrapper tag, so it is never anything but this set.
- Absent: this client never marks a turn. A `hello` without the field encodes
  exactly as before; an older daemon ignores the field.
- SynapsCLI itself hardcodes no label. Which client may mark, and under which
  name, is whatever the owner's chat client declares.

## The `origin` field

`Submit` and `Steer` take an optional `origin` string, inside the existing
session command envelope:

```json
{"cmd":"submit","text":"hi","origin":"my-app"}
{"cmd":"steer","text":"and this","origin":"my-app"}
```

- Absent: today's behaviour. The command and the request are byte-identical to
  a build without this feature.
- A valid label (the set above) counts only when the sender is attached as
  a client whose `hello` declared `client.kind` `chat` AND declared this very
  label in `client.turn_origin`, and one of these holds:
  - the session is owned and the sender is its owner (the client bound with
    the owner token the create returned);
  - the command came on the owner app's bound connection, in a frame that
    the registered app process wrote after its claim was checked (see
    [The owner claim](#the-owner-claim)). This holds in any session, owned
    or not, and it is the only way a session without an owner marks a turn.

  From the owner attached as any other kind (`tui`, `attach`, `rpc`,
  `server`, `test`) or as a chat client that declared no label or another
  one, from any other client in a session without an owner, or for a
  command no attached client sent, the command is NOT refused: it runs
  exactly as without an origin, so its text goes in unmarked. One warn log
  line names the command, the sender's kind, whether it is the session's
  owner, whether its frame came from the bound app, the frame writer's pid
  and exe when the daemon saw them, and the reason (`no_client`, `not_chat`,
  `label_mismatch` or `not_owner`), never the label or any text. An
  admitted mark logs one info line naming the command, the path (`via`
  `session_owner` or `owner_claim`) and the writer's pid and exe. In an
  owned session a client other than the owner may not send a `Submit` or
  `Steer` at all (the owned-session rule refuses it, on the bound
  connection too), so it can mark nothing either.
- Any value that is not a valid label refuses the command, from any sender.
  Nothing is queued, no turn starts, and the sender gets a `refused` event
  (`command` `submit` or `steer`, `reason` `invalid turn origin`). The log
  line names the command, never the value. A string, not an enum, so a bad
  value is a refusal, not a malformed frame.

Only `Submit` and `Steer` carry an origin, and only on text the owner typed:
a marked `Submit` with no text (only attachments) marks nothing. A session's
create-time first message, `SubmitPrepared`, subagent tasks and steers,
events and compaction successors never do. A message that arrives in
`SubmitPrepared` loses every host key (`_synaps_` and the rest of the name: a
mark, a host-notice mark, a context marker) before it is stored.

A marked `Steer` keeps its origin in the message it becomes: delivered into
the running turn, or queued and starting a turn later (the wrapper is rendered
once, not twice). A session queues one text at a time: a later queued text
replaces the earlier one, never joins it, and the message it becomes is marked
only when its text is exactly the text a marked command queued.

## What the model sees

A marked text goes to the model as: the opening tag (the word synaps-turn with
the attribute origin set to the client's label, in angle brackets), a newline,
the owner's text (neutralized), a newline, and the closing tag. With
attachments, the wrapper goes around the text block only; images and
documents stay outside it.

A request that renders a wrapper also gets one fixed sentence, appended to
its system prompt, that says what the wrapper proves. A request with no
wrapper gets none.

The mark is stored beside the message (in memory, in the journal and in
snapshots), never inside its text. The wrapper is rendered only while a
provider request is built, in one place that every provider goes through
(`request_view` at each request entry: `call_api_stream_inner` and
`call_api_simple`). Views, history, journals and snapshots never hold the
wrapper. Reload, rehydrate, replay and fork render the same request. A turn
marked while a session had its owner keeps its mark when the saved session is
continued, by any client. A journal written with origins loads in an older
build without truncation, and that build sends no extra key and no wrapper.

## Neutralizing

Every text a request sends is neutralized, the owner's own marked text too
(an early close followed by a forged open, say): user text, `SubmitPrepared`
messages, steers, event bodies, tool results (nested and MCP), memory and
subagent output, subagent tasks, compaction summaries, assistant text, text
documents, extension context, and every string of a block of a kind
SynapsCLI does not know (a transport may pass it on). Two things are replaced:

- Anything that could read as the wrapper's opening or closing tag has its
  one opener char replaced with `&lt;`.
- A host notice opener (a `[` that starts one of the notice headers below) at
  the start of a line has its `[` replaced with `&#91;`. Only a host notice
  keeps its own: a part the host marked as its own when it wrote it
  (`_synaps_notice`, `{"parts": [N, ..]}`: the extension-context block a turn
  carries), a saved extension block a `before_message` hook added to a user
  message (one of the blocks `_synaps_hooked` counts, framed exactly as the
  host frames it, its content neutralized when it was saved), or the text of
  a context continuation or a worker context rollover (each marked by its
  own schema key). Their tags are still
  neutralized, and every text they quote is neutralized when they are
  written.

Every other byte stays as sent, and neutralizing twice changes nothing more.
The history keeps the text as it was typed or returned.

The tag rule:

- The opener is the less-than sign or a lookalike of it: its fullwidth and
  small forms, the single angle quotation mark, the mathematical, CJK and
  other left angle brackets, the modifier letter and Canadian syllabics forms,
  the chars Unicode's confusables data (confusables.txt 18.0.0) maps to it or
  to the angle bracket U+276C, and the signs that data reads as several chars
  ending in it, such as U+226A.
- Lookalikes are folded for matching only: fullwidth forms to ASCII,
  closer lookalikes to the greater-than sign, fraction and division slashes
  to `/`, every hyphen and dash lookalike to `-` (U+2010 to U+2015, the minus
  signs, the small and fullwidth hyphen-minus, the modifier letter minus
  U+02D7, the hyphen bullet U+2043, the small em dash U+FE58, the katakana
  prolonged sound mark U+30FC, the heavy minus U+2796, the Armenian, Hebrew,
  Mongolian and Canadian hyphens, the wave dashes and others: `is_dash` in
  `runtime/turn_origin.rs`), letter lookalikes to the Latin letter (a table
  of Cyrillic, Greek and Armenian letters, mathematical alphanumerics,
  circled and small-capital letters, letterlike symbols and Roman numerals:
  `confusable`), then ASCII case. So an upper-case, fullwidth or mixed-script
  spelling matches. Spaces are not folded: wherever a space is allowed, any
  Unicode whitespace (the ideographic and no-break spaces too) counts.
- From the opener: optional whitespace, an optional `/`, optional whitespace,
  then the word synaps-turn. Invisible chars (zero-width, bidi marks and
  overrides, the soft hyphen, the Hangul fillers, variation selectors, tag
  chars: the list in `events/format.rs`) and control chars are skipped
  anywhere from the opener through the word's last letter, and a soft hyphen
  stands for the word's `-` when no dash follows it. Plain whitespace inside
  the word ends the match.
- Right after the word, the next char must be the end of the text,
  whitespace (a newline or tab counts), an invisible or control char, `/`,
  or the greater-than sign or a lookalike of it: its fullwidth and small
  forms, the partners of the opener's quotation mark, brackets and letter
  forms, the chars that data maps to it, and the signs it reads as several
  chars starting with it, such as U+226B. A char folds one way only: U+2AA5,
  which that data reads as `><`, counts as an opener.
- Runs: adjacent text parts are read back to back, within a message, a tool
  result and across messages (a later merge joins consecutive same-role
  messages, so a tag split across two steers is caught), so a tag split
  across parts is caught. The opener char is replaced in whichever part holds
  it; part boundaries never move. An image, document or tool_result block
  breaks the run. Each part is also read alone, and runs also read through
  thinking and tool_use blocks and unknown blocks, because some transports
  (OpenAI-compatible) join the text around them.

The notice openers are `NOTICE_OPENERS` in `events/format.rs` (the headers
the host's continuation, extension context, end and relay notices start
with), plus any the `SYNAPS_NOTICE_OPENERS` environment variable adds (a `|`
separated list, add-only; nothing removes one). They are matched through the
same folding (letter and dash lookalikes, case, a run of whitespace for a
space, invisible chars skipped, a soft hyphen for a `-`). The start of a line
is the start of the text or the char after a line break, past whitespace,
invisible chars and a leading run of quote, list and heading markers: `>`,
`-` and every dash, `*`, `+`, `#`, a number with its `.` or `)`, and the
common bullets. A host notice opener later in a line is left alone. Event
bodies go through the same neutralizer (`neutralize_notice_openers`,
`NoticeScope`), so event text cannot pose as a host notice either.

The texts MCP servers and extensions supply to the system prompt or the tool
list are neutralized where SynapsCLI takes them in: MCP and extension tools'
descriptions (and the `description` and `title` strings of their input
schemas) in the request's tool list, the list of activatable tools (every
line, its notice openers anywhere in it), and an extension's context:
`on_session_start`'s and `before_message`'s, saved and turn-only (inside its
fence, whose own open and close the host writes after it).

Not neutralized:

- thinking blocks (they are signed; changing a byte voids the signature);
- tool_use inputs (the arguments the model wrote);
- image bytes and PDF bytes (text inside them is not text to SynapsCLI);
- the base system prompt, the host's own text (a client that composes layers
  into it neutralizes each layer itself);
- the fixed sentence above, which names the tag on purpose;
- builtin tools' descriptions, and the values and enums of any tool's input
  schema.

Left alone on purpose: escaped spellings are not the tag (a JSON escape such
as `\u003c`, HTML entities such as `&#60;` and `&lt;`, a URL escape such as
`%3C`). One named residual also passes: a plain space inside the word. The
letter table is the letters of the tag word and of the notice openers in the
scripts above, not all of confusables.txt. Only the wrapper SynapsCLI adds
counts.

## Named limits

Each of these loses a mark; none adds one. The text then goes in unmarked
(fail closed).

- Text waiting in the driver's FIFO loses its origin. A text queued behind a
  compaction keeps it only while it is the one queued text (see above).
- A full resnapshot written by an older build drops the origins of the
  messages it holds.
- A queued message restored on reload is unmarked.

Rolling SynapsCLI back below this feature turns marks off, and that build
also does not neutralize. A client can tell from `Welcome.features`: without
`turn_origin`, treat any wrapper as unproven.

## The owner claim

`Welcome.features` lists `turn_origin_owner` whenever the build has the
claim, configured or not.

**Seams.**

- The owner app's path: env `SYNAPS_DAEMON_OWNER_APP_EXE` (the app sets it
  in the environment it starts the daemon with, to a stable path such as a
  `current` link to its installed image), or config `daemon.owner_app_exe`.
  **The env wins** whenever it is set and non-empty; the config value is then
  ignored. Both are read once, at daemon start, and the env value is then
  removed from the daemon's own environment, so no child the daemon starts
  inherits it (a reload hands it to the next image). `main` moves it aside
  before it builds the tokio runtime, while the process has one thread, so
  the removal races no `getenv`; the daemon's start takes it from there, and
  a client that starts a daemon hands it on. The path is resolved to (dev,
  ino) at each claim, so an install that repoints `current` needs no daemon
  restart. Neither set: every claim answers `unsupported`.
- The settle window G: `SYNAPS_DAEMON_OWNER_SETTLE_SECS` (default 15), read
  at daemon start.

**Frames.**

- Client: `{"type":"claim_owner","key":"<64 lower-case hex>"}` after
  `welcome` and before `attach` (an attached link may claim too), once per
  link, and again only after a pushed unbind (`contested`, `reset`). A second
  claim while the link's is live gets `error` "owner claim already made on
  this connection"; a key that is not exactly 64 lower-case hex digits gets
  `error` "malformed frame: claim_owner key must be 64 lower-case hex digits
  ...". Both are non-terminal, never echo the key, and carry the attached
  session's id on an attached link. Before `welcome` it is refused as any
  frame before `hello` is (`refused`, reason `protocol`, terminal).
- Daemon: `{"type":"owner_claim","owner_bound":<bool>,"reason":"<reason>"}`,
  `reason` absent when bound: `settling`, `contested`, `not_the_app`,
  `not_the_registrant`, `reset`, `unsupported`. Pushed later on a link that
  claimed: `owner_bound:true` once G passes (to links that proved the key),
  `contested`, `reset`. Nothing is pushed when the registrant exits (its own
  links close with it; every link may claim again). A reply or push that
  finds the link's queue full is held and sent as soon as there is room,
  in order; a later one replaces a held one (each states the link's whole
  claim), so it is never dropped while the link lives and the link always
  ends on the current state.
- The app should wait for the reply to its `claim_owner` before it sends a
  marked turn: a turn written before the daemon checked the claim is never
  the owner's (see "Per frame").
- Every frame's write to a client is bounded (60 s): a client that stops
  reading loses its connection, so no reply or push waits on it for good.
- `attached.owner_bound`: present only on a link that claimed (whether it is
  bound now); absent means false.
- Only links that sent `claim_owner` ever get an `owner_claim` frame (plus the
  reply to `owner_reset`): an older client's frame decoder may be closed and
  drop the link on an unknown type.
- `{"type":"owner_reset"}` (`synaps daemon owner reset [--json]`) answers
  `owner_claim{false, reset}` and pushes the same to every other link that
  claimed.

**The check, at each claim.** The claimant is the process that wrote the
claim's bytes (its tgid from `SCM_CREDENTIALS`; the daemon sets
`SO_PASSCRED`). Its `/proc/<pid>/exe`, stat'd while a pidfd pins it, must be
the very file (dev, ino) the path names now. An exe that can't be read or
stat'd fails closed (`not_the_app`): **the app must stay dumpable** (a
non-dumpable process's `/proc` entries are root's). A claimant started with
`LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH`, `WEBKIT_INSPECTOR_SERVER`,
`WEBKIT_INSPECTOR_HTTP_SERVER` or `TAURI_WEBVIEW_AUTOMATION` set and
non-empty, or whose environ can't be read, is `not_the_app`. The check comes
before any state is read, so another program can neither contest nor learn
the state.

**States.** Vacant: the first claim that passes registers {the key's
SHA-256, the claimant's pid and start time, the image's dev/ino} and goes
Settling for G, timed from that registration; then Bound. A second key while
Settling or Bound: Contested, which means nobody is bound until a cold start
or `owner reset`. The right key from another process: `not_the_registrant`,
told only to that link; the bound app stays bound. The registrant's exit:
Vacant at once. While it lives the registration holds, links or none.
`owner reset` only denies: Vacant, and nothing binds by itself.

**Per frame.** A `Submit`/`Steer` origin counts as the owner app's only when
Bound, its link proved the key, the frame starts at or past the link's
mark, its bytes came from the registrant's tgid, still running, and that
process's `/proc/<pid>/exe` is still the image the claim checked (stat'd
again for each such frame). A frame whose bytes came from more than one
process counts as no one's. It then still needs the chat kind and the
declared label (see [The `origin` field](#the-origin-field)).

The mark is the link's socket offset when its claim passed the image
check: the bytes read so far plus those still queued in the socket
(`FIONREAD`), read after the check. A frame that starts before it was
written before the check. So a process that writes a claim with a key of
its own and a marked turn behind it, then `execve`s the app image in place
(same pid, so the claim's writer passes the check as the app), gets no
queued turn counted; after the `execve` the process runs the owner app's own
code, and a later `execve` to another image fails the per-frame stat. A
link whose queued bytes can't be measured fails the claim closed
(`not_the_app`). `SO_PASSPIDFD`/`SCM_PIDFD` is not used: a pidfd names the
process, not its image, and stays the same across an `execve`; the pidfd
and start time the claim keeps already rule out a reused pid.

**Reload.** The state, the SHA-256, the registrant's pid and start time and
the image's dev/ino ride the reload state (never the key). The new image
runs the image check on the carried registrant again, against the app path
as it reads it (the file's dev/ino now, the start environ), as the same
instance (start time); gone, another image or another instance: Vacant.
Links prove the key again (each gets a new mark); Contested stays
Contested; a carried Settling starts a full G.

**The key.** The daemon decodes it into a heap buffer boxed inside a
wrapper that wipes on drop (so moving the key leaves no copy of it behind),
hashes it and wipes it before the verdict, and wipes the frame's line
buffer after the read; it keeps only the SHA-256. Logs name the pid, exe
and verdict per claim, never the key or its digest.

## What the mark proves

The mark proves that a chat client that declared that label in its `hello`
sent the text as typed input, and that it was either:

- the owned session's owner: the client holding the owner token the
  session's create returned; or
- the process registered as the owner app: its image is the file the owner
  app path names, it proved the key the registrant holds in memory, the
  claim was Bound, and the turn's bytes were written by it after the claim's
  check.

Another client, even one running as the same user on the same socket and
declaring the same label, cannot get a turn wrapped through either path: in
an owned session it may not drive the session at all, and in a session
without an owner its frames are not the registered app's. Text from tools,
files, web pages, other clients, workers, summaries and the model itself
cannot carry the mark, and cannot pose as a host notice at the start of a
line.

The claim does not prove what the registered process runs (see "Not
covered" below). Any process running as the owner can reach the daemon
socket (a sandboxed worker, since the sandbox cannot limit connect; an
unconfined session's tools; any local program). The claim narrows marking
in a session without an owner to the registered app process; same-uid code
can still add marks through the gaps listed below, and can deny marks
(contest, reset).

The proof stops at the session's saved files: a process running as the
owner outside the write sandbox can write a marked message into a saved
session, and it shows as the owner's after a reload. The owner token is as
safe as the owner's client keeps it.

## Not covered

- The daemon records no client identity per marked turn, so past marks
  can't be traced to a client.
- A same-uid process that repoints the owner app's `current` link, rewrites the
  environment the app starts the daemon with or `daemon.owner_app_exe`, or
  swaps the app image can register its own binary whenever the owner claim
  is Vacant (after the app exits, or after an owner reset).
- Any client's `Reload{exe}` (the control loop's `Reload` arm in
  `daemon/conn.rs`; `validate_exe` checks only same-uid ownership and that
  the file is not group- or world-writable) replaces the daemon with its own
  binary. This predates the claim.
- After the app exits the claim is Vacant, and a same-uid process can take
  it with the genuine app image. It can start the image under its own
  control (its own child, its args, an env the check doesn't refuse), which
  then claims with the key that app holds and does what its controller
  makes it do. Or a script can write the claim itself, with a key it picked,
  and `execve` the image in place so the claim's writer passes the check as
  the app: the script holds the key, and any text it wrote behind the claim
  is its own. The link's mark ("Per frame") keeps every frame written before
  the check from counting, and the per-frame exe stat keeps a later
  `execve` to another image from counting; what the genuine image itself
  writes on that link afterwards counts. A child the app starts on an
  inherited copy of its connection's fd is not the registrant, so its
  frames go plain, but whatever the app itself writes on behalf of such a
  child counts. Input injected into the app window, and root, are not
  covered either.
- An app build whose UI is served by a development server: whoever edits
  the served tree or serves that port runs code in the bound app's webview
  at any time, not only while Vacant. A release build with a bundled UI
  closes this path. No env var or argument of such an app has to select the
  UI, so none is checked.
- The refused env list is best-effort, not a sandbox. A process can blank
  its own environ entry (the start env block `/proc/<pid>/environ` reads is
  in its own memory), so a loader var it was started with need not show.
  Not listed, though each loads code into the app or points it elsewhere:
  `GTK_MODULES`, `GTK3_MODULES` and `GIO_EXTRA_MODULES` (an app may set
  them itself), `GTK_PATH`, `GTK_IM_MODULE_FILE`, `GDK_PIXBUF_MODULE_FILE`,
  `GIO_MODULE_DIR`, `GCONV_PATH`, `GST_PLUGIN_PATH*`, `VK_*` (such as
  `VK_LAYER_PATH`, `VK_ICD_FILENAMES`), EGL/LIBGL
  (`__EGL_VENDOR_LIBRARY_FILENAMES`, `LIBGL_DRIVERS_PATH`), and an
  attacker's `DISPLAY`, `WAYLAND_DISPLAY` or `DBUS_SESSION_BUS_ADDRESS`.
- The environ check reads the start env: a later `setenv` in the app isn't
  seen. A process allowed to ptrace the app (root, `CAP_SYS_PTRACE`, Yama
  `ptrace_scope` 0) can drive it.
- A process with `CAP_CHECKPOINT_RESTORE` in its user namespace can repoint
  its own `/proc/<pid>/exe` at the app image (`prctl(PR_SET_MM,
  PR_SET_MM_MAP)` with an `exe_fd`) while it runs other code, and so pass
  the image check and the per-frame stat. An unprivileged user gets that
  capability by creating a user namespace where the host allows
  unprivileged user namespaces (some hosts restrict them, for example with
  AppArmor; not every host does).
- Any same-uid process can run `owner reset` (denial only).
- Key bytes the daemon can't wipe: the `BufReader`'s internal buffer and the
  kernel's socket buffer until reused; old allocations of the line buffer
  when a long frame made it grow; serde_json's scratch copy when the key
  string holds JSON escapes.
- A link answered `not_the_registrant` is not told when the registrant
  exits; it binds by claiming again on a new link.
- macOS and Windows: no image check, so every claim is `unsupported`, and a
  session without an owner marks nothing there. Neither is measured.

## Verification

The tests of the mark have `turn_origin` in their names, so
`cargo test -- turn_origin` runs them; the persistence, notice-opener and
supplied-text tests are named by module below. `runtime::turn_origin` unit
tests pin every row of the neutralizer
vectors (neutralized, left alone, split across parts and across messages,
inside unknown blocks), the notice openers in every kind of text and their
host-notice exemption, the render rules, the sender gate (owner or bound
app, kind and declared label) and its log lines, that only the daemon's
connection code can say a frame is the bound app's, the vouch sentence's exact bytes, and a
scan that no file under `crates`, `src`, `docs` or `tests` spells the tag.
`runtime::turn_origin_entry_tests` checks that every request entry renders
once and neutralizes, on the Anthropic and the chat-completions wire, and
that the vouch is added after a wrapper and never otherwise.
`session_actor_turn_origin` covers the wire path in an owned session (marked,
forged, with an image, an attachment without text, refused, from a sender
that is not the owner's declaring chat client and from any sender in a
session without an owner, a non-owner in an owned session, a mid-turn steer,
`SubmitPrepared`, a continued session) and, in sessions without an owner,
the owner app's bound connection against one that is not bound, one with
no connection behind it and one with no label; `daemon::owner_claim` tests
(`cargo test -- owner_claim`) pin the claim's states, reasons, image and env
checks, lapse, reload carry, logs and key wipe; `daemon::frame_creds` tests
pin the per-frame writer and the socket offset; `daemon::conn` tests pin the
line wipe and the bounded write; `tests/daemon_owner_claim.rs` drives the
claim over a real socket; the `session_journal` tests cover
persistence, older-build loading and malformed stored marks; the
`events::format` notice-opener tests cover the notice neutralizer, the line
leads and the lookalikes; and the extension-context, activatable-list and
tool-schema tests cover the supplied texts.
