# Turn origin

`Welcome.features` advertises `turn_origin`.

A turn origin tells the model which client wrote a user turn. A chat client
that speaks for the owner (a desktop app with a composer, say) creates an
owned session, declares a short label in its `hello` and marks the turns the
owner types in its own input box with that label. Text it marks reaches the
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
- A valid label (the set above) counts only in an owned session, and only
  when the sender is that session's owner (the client bound with the owner
  token the create returned) attached as a client whose `hello` declared
  `client.kind` `chat` AND declared this very label in `client.turn_origin`.
  A session without an owner never marks a turn: any client may declare the
  label there, and its text still goes in plain. From the owner attached as
  any other kind (`tui`, `attach`, `rpc`, `server`, `test`) or as a chat
  client that declared no label or another one, from any client in a session
  without an owner, or for a command no attached client sent, the command is
  NOT refused: it runs exactly as without an origin, so its text goes in
  unmarked. One warn log line names the command, the sender's kind and
  whether it is the owner, never the label or any text. In an owned session a
  client other than the owner may not send a `Submit` or `Steer` at all (the
  owned-session rule refuses it), so it can mark nothing either.
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

## What the mark proves

The mark proves that the owned session's owner sent the text as typed input:
the client holding the owner token the session's create returned, attached
as a chat client that declared that label in its `hello`. Another client,
even one running as the same user on the same socket and declaring the same
label, cannot get a turn wrapped: in an owned session it may not drive the
session at all, and a session without an owner never wraps one. Text from
tools, files, web pages, other clients, workers, summaries and the model
itself cannot carry the mark, and cannot pose as a host notice at the start
of a line.

The proof stops at the session's saved files: a process running as the
owner outside the write sandbox can write a marked message into a saved
session, and it shows as the owner's after a reload. The owner token is as
safe as the owner's client keeps it.

## Verification

The tests of the mark have `turn_origin` in their names, so
`cargo test -- turn_origin` runs them; the persistence, notice-opener and
supplied-text tests are named by module below. `runtime::turn_origin` unit
tests pin every row of the neutralizer
vectors (neutralized, left alone, split across parts and across messages,
inside unknown blocks), the notice openers in every kind of text and their
host-notice exemption, the render rules, the sender gate (owner, kind and
declared label) and its log line, the vouch sentence's exact bytes, and a
scan that no file under `crates`, `src`, `docs` or `tests` spells the tag.
`runtime::turn_origin_entry_tests` checks that every request entry renders
once and neutralizes, on the Anthropic and the chat-completions wire, and
that the vouch is added after a wrapper and never otherwise.
`session_actor_turn_origin` covers the wire path in an owned session (marked,
forged, with an image, an attachment without text, refused, from a sender
that is not the owner's declaring chat client and from any sender in a
session without an owner, a non-owner in an owned session, a mid-turn steer,
`SubmitPrepared`, a continued session); the `session_journal` tests cover
persistence, older-build loading and malformed stored marks; the
`events::format` notice-opener tests cover the notice neutralizer, the line
leads and the lookalikes; and the extension-context, activatable-list and
tool-schema tests cover the supplied texts.
