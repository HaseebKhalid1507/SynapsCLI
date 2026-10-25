# Watching and steering a subagent

New in 0.11.0. Any running subagent can be opened in its own thread view in the TUI.
It renders like the main conversation (text, thinking, tool calls and results,
streamed live), and anything you type there goes to that subagent.

## Before you start: reload the daemon

Watch and steer are new daemon commands. A daemon started by an older build can't
serve them. After you install 0.11.0:

```bash
synaps daemon reload      # moves the running daemon onto the new binary
```

Then restart any attached TUIs. Until you reload, a new TUI attached to an old
daemon shows this once in the worker view:

> the attached daemon predates subagent watch — run `synaps daemon reload` (after
> installing this build) to enable it

Everything else keeps working. Only watch and steer are unavailable.

## Keys

| Key | What it does |
|---|---|
| **Ctrl-↑** | From the main thread, opens the newest subagent (the bottom row of the subagent tray). Press again to walk to older ones. |
| **Ctrl-↓** | Walks back toward newer subagents. Past the newest, returns to the main thread. |
| **Esc** | Returns to the main thread. In a worker view it never cancels the main turn, and a quick second Esc isn't treated as one either. |
| **Enter** (with text) | Steers the subagent you're watching. |

Ctrl-↑ and Ctrl-↓ are reserved core keys. `keybind.*` entries and plugin keybinds
can't use them.

**macOS:** by default, Ctrl-↑/↓ open Mission Control and App Exposé before the
terminal sees them. Use `/watch` there. Alt-↑/↓ (Option; in Terminal.app turn on
"Use Option as Meta key") do the same as Ctrl-↑/↓. Alt-↑/↓ lands with the TUI
polish PR (`fix/tui-release-polish`).

## `/watch`

```
/watch                 list the subagents you can watch (opens it if there's only one)
/watch <name>          open a subagent by agent name
/watch sa_3            open by handle id
/watch 3               same, short form
/watch main            back to the main thread (also: off, back, exit)
```

The list shows each subagent's handle, name and status, and marks the one you're
watching. You can watch running subagents, plus any finished one whose view is
still open.

## Steering

Type in a running subagent's view and press Enter. The text goes to that subagent
as a steer, not to the main agent. The main conversation also gets a note that
you redirected its worker, so the main agent knows.

- Only the client that owns the input can steer. Observers can watch.
- Slash commands typed in a worker view still run, and their output appears in
  that view.
- A finished subagent's view is read-only. It stays watchable for 15 minutes.

## What you see

- Scrolling, selection and Ctrl-O act on the view you're in. The main thread
  keeps its own scroll position while you're away.
- Subagent rows and watch pages show the worker's model.
- A worker view keeps the last 400 messages (up to 2 MiB).
