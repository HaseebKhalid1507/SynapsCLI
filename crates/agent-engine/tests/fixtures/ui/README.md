# Agent card fixtures (`synaps:card/1`, C1)

The corpus for the card model in `crates/agent-engine/src/ui/`
(`agent-ui/architecture.md` v3.1, §5 and Appendix A). The engine's
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
| `answers[]` | `values` a client submits (`{}` for a skip), with `card`, `answer_id`, `status`, `rev`; `message` is the exact `<ui_answer>` text the model gets; `late`, when present, is a late delivery: `turn_message` (the message that started the card's turn), its `task` excerpt, and the `asked` capsule |
| `bad_answers[]` | `values` refused, with the exact `error` line |
| `wait_result` | (A2) the waiting tool's result: the same `answer` object |
| `shown` | (A1) the live tool result |

A close (`a5_close.json`) has `input`, `close` (the id) and `result` instead.

## `invalid/*.json`

`input` is refused with exactly `error` (`<path>: <one teaching line>`). The
`a_*` files are Appendix A's invalid list.

## `sanitize.json`

`ui_sanitize` parity (§4.3). For each `in`: `single` (a one-line field: `\n`
and `\t` become spaces), `multi` (`detail`, `long` values: `\t` becomes a
space), and `refused`: the first character validation refuses in an
agent-written string (`{char, why}`), or `null`. A refused string never
reaches sanitizing; `single` and `multi` still show what display
sanitizing does with it (receipts, roster labels, defense in depth).
