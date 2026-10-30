# Confirm prompts (extension tool-call consent)

An extension with `tools.intercept` can answer a `before_tool_call` hook with
`confirm`. Synaps then stops before running the tool and asks you. This page
describes the prompt as it ships in 0.11.0.

> **Lands with the consent PR.** The behaviour below is the final behaviour from
> `fix/consent-latch-broker-exposure`. Before that PR, "Allow all" was one switch
> for every extension and had a letter key (`a`), and `/confirms` didn't exist.

## The dialog

The dialog shows, above the extension's own message, which **extension** asked
and for which **tool**. The host fills both in, so an extension can't fake them.
If an earlier extension rewrote the tool input, the dialog also shows
"Input rewritten by: <ids>" and a preview of the input that will actually run.

There are three buttons:

| Button | Effect |
|---|---|
| **Allow** | Runs this one call. |
| **Allow all from `<ext>` this session** | Runs this call and auto-approves later confirms **from that extension only** (any tool) for the rest of the session. |
| **Deny** | The tool doesn't run. |

Keys:

| Key | Effect |
|---|---|
| `y` | Allow |
| `n` or Esc | Deny |
| Tab, ←, → | Move between buttons. Focus doesn't wrap. |
| Enter | Press the focused button. **Deny is focused when the dialog opens.** |

"Allow all" has no letter key, on purpose. Focus it, press Enter, and the hint row
changes to "Press Enter again to allow all from `<ext>` for this session · Esc
cancels". Press Enter a second time to confirm. Esc or moving focus cancels just
that step.

The dialog ignores keys for 400 ms after it opens, and every key you press in that
time restarts the wait. So if you were typing when it popped up, your keystrokes
won't answer it. Held-down keys (auto-repeat) are always ignored.

## What "Allow all" covers

- Only the extension it names, for any tool, until `/new`, `/confirms reset`, or
  that extension is reloaded or reinstalled.
- Confirms from other extensions still prompt.
- A `block` from any extension still wins. "Allow all" never overrides a block.
- When several extensions confirm the same call, you get one prompt that names
  all of them, and "Allow all" covers each of them. A later confirm is
  auto-approved only when every extension that raised it is covered.
- A confirm the host can't attribute to an extension never offers "Allow all".
- Every attached client sees a notice saying who armed "Allow all" and for which
  extension.

## Who can answer

- Only the client that owns the input can choose "Allow all". Other mirror
  clients see Allow and Deny only. If you lose input ownership while a prompt is
  open, its "Allow all" button goes away.
- `--attach --observe` clients can't answer confirms at all.
- `synaps --attach --no-confirm-answers` opens a TUI that never answers a
  confirm, which is useful for embedded terminals.

## `/confirms`

```
/confirms            list the extensions that have "Allow all" this session
/confirms status     same
/confirms reset      revoke "Allow all" for every extension, subagents included
```

`/new` also revokes it, including for subagents that are still running. Like the
other engine commands, `/confirms` is owner-only from an attached client.

## Subagents

Subagent tool calls **don't run extension `before_tool_call` hooks** in this
release. So extension `confirm` and `block` don't apply to subagents at all. A
subagent's tool call isn't prompted and isn't blocked by an extension, whatever
you answered in the main thread. The only hooks a subagent runs are output
transforms (`after_tool_call` `replace` from extensions with
`tools.transform_output`), which never prompt; see
`docs/extensions/hooks.md` ("Subagent workers").

## Turning prompts off

`server.auto_approve_confirms = true` approves every extension confirm without
asking. Use it only where nobody is there to answer, and only with extensions you
trust.

For extension authors, see `docs/extensions/protocol.md` (the `confirm` action
and "Handler chain order").
