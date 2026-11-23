# Agent card fixtures (`synaps:card/1`, C1)

The corpus for the card model in `crates/agent-engine/src/ui/`
(`agent-ui/architecture.md` v3.2, §5 and Appendix A). The engine's
`crates/agent-engine/tests/ui_card_model.rs` runs every file; the desk's D1
copies this directory and runs its mirror against it. Each file stands alone:
no file refers to another, and nothing here depends on the engine's code.

Invisible and control characters are written as `\u` escapes so a reader can
see them; they are ordinary JSON strings.

## `valid/*.json`

A `ui` input that is accepted, and what the host makes of it:

| key | meaning |
|---|---|
| `input` | the tool input, as the agent sends it |
| `doc` | the normalized `CardDoc` (sanitized, defaults materialized, string options as `{label}`, empty optional strings absent) |
| `wait` | the call's `wait` |
| `mode` | `quick` or `form` (§5.2) |
| `contract` | `"c1:" + hex128(sha256(canonical preimage))` (§5.4) |
| `answers[]` | `values` a client submits (`{}` for a skip), with `card`, `answer_id`, `status`, `rev`; `message` is the exact `<ui_answer>` text the model gets, and `q` the question as that message's attribute reads back (the event renderer's projection); `late`, when present, is a late delivery: `turn_message` (the message that started the card's turn), its `task` excerpt, and the `asked` capsule |
| `bad_answers[]` | `values` refused, with the exact `error` line |
| `wait_result` | (A2) the waiting tool's result: the same `answer` object |
| `shown` | (A1) the live tool result |
| `input_bytes`, `doc_bytes` | (`max_size_*`) the input is exactly at its 8 KiB cap and normalizes to a doc of `doc_bytes`, over 8 KiB and within `DOC_MAX_BYTES` (9588); the doc must revalidate |

Every doc must also revalidate: read back, it is exactly what normalizing
makes of it. A close (`a5_close.json`, `close_with_defaults.json`) has
`input`, `close` (the id) and `result` instead.

## `invalid/*.json`

`input` is refused with exactly `error` (`<path>: <one teaching line>`). The
`a_*` files are Appendix A's invalid list.

Every refusal, here and in `bad_answers`, is one line of at most 322
characters with no control, bidi control or invisible character: a key the
agent or a client wrote appears escaped (`"\u001b[31m"`) and cut to 24
characters.

## `raw/*.json`

JSON text through the shared ingress (`decode_json`), then the model. A
repeated key at any level is refused at decoding (never last-wins), as are
lone surrogates, numbers out of `f64` range and nesting past 128.

| key | meaning |
|---|---|
| `what` | `input` (a `ui` input) or `answer` (values for the card `card`) |
| `text` | the raw JSON |
| `json_error` | the decoding refusal's prefix, or `null` when it decodes |
| `doc` / `values` / `error` | what the model makes of the decoded value |

## `casefold.json`

The key labels are compared by, for "labels must differ (ignoring case)"
and for matching `default` to a label: Unicode full case folding
(`CaseFolding.txt` statuses C and F, Python's `str.casefold`) of the
sanitized label, with no other normalization (no NFC). Every scalar value
folds to its full lowercase mapping except the ones listed, which fold to
the listed string. `unicode` is the version the table was generated at.

## `sanitize.json`

`ui_sanitize` parity (§4.3). `\r\n` and a lone `\r` are line endings: they
become `\n` before anything else (v3.2 #1). For each `in`: `single` (a
one-line field: `\n` and `\t` become spaces), `multi` (`detail`, `long` values: `\t` becomes a
space), and `refused`: the first character validation refuses in an
agent-written string (`{char, why}`), or `null`. A refused string never
reaches sanitizing; `single` and `multi` still show what display
sanitizing does with it (receipts, roster labels, defense in depth).
