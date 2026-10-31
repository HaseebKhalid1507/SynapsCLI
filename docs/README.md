# SynapsCLI Documentation

Durable references live here at the top level. Working history is filed by kind in the subdirectories.

## Reference

| Doc | What it covers |
|---|---|
| [STABILITY.md](STABILITY.md) | Stability and compatibility policy, including the extension protocol version. Pinned by `tests/contracts_sync.rs`. |
| [architecture-host-plugin-split.md](architecture-host-plugin-split.md) | How the host and plugin halves divide responsibility. |
| [rpc-protocol.md](rpc-protocol.md) | `synaps rpc` wire protocol. |
| [daemon-mode.md](daemon-mode.md) | `synaps daemon` and `synaps --attach`: lifecycle, pins, rename, reload, files and the wire protocol (Unix socket or Windows named pipe). |
| [sidecar-protocol.md](sidecar-protocol.md) | Sidecar protocol v2. |
| [events-reactor.md](events-reactor.md) | Runtime event reactor semantics and mode policy. |
| [petri-net-model.md](petri-net-model.md) | Petri-net model of a daemon session, a turn, a tool batch, a tool call and the prompt cache, with the invariants each net encodes. |
| [trace-schema.md](trace-schema.md) | `synaps-request-trace/1` schema. |
| [request-lifecycle-progressive-disclosure.md](request-lifecycle-progressive-disclosure.md) | Opt-in progressive tool disclosure. |
| [cloud-oauth-providers-runbook.md](cloud-oauth-providers-runbook.md) | Operational runbook for the cloud OAuth providers. |
| [open-provider-issues.md](open-provider-issues.md) | Known open provider gaps. |
| [subagent-watch.md](subagent-watch.md) | Watching and steering a subagent in its own TUI view. |
| [subagent-thinking.md](subagent-thinking.md) | Worker thinking effort: the per-call `thinking` argument and the `subagent_thinking` default. |
| [multi-edit.md](multi-edit.md) | The `multi_edit` tool: anchored edits across files in one call, all or nothing. |
| [sudo-askpass.md](sudo-askpass.md) | How the bash tool asks for a sudo password (Linux). |
| [confirm-prompts.md](confirm-prompts.md) | Extension tool-call Confirm prompts, "Allow all", `/confirms`. |
| [providers/](providers/) | Provider guides: Meta Muse, multi-account broker and usage, quota keeper, Codex. |
| `tools.json` | Committed builtin tool manifest. Drift-checked by `tests/tools_export.rs` — regenerate with `synaps tools export --pretty`. |

## By kind

| Directory | Contents |
|---|---|
| [specs/](specs/) | Design specifications. |
| [plans/](plans/) | Implementation plans. |
| [reviews/](reviews/) | Code and architecture review records. |
| [decisions/](decisions/) | Accepted decision records. |
| [design/](design/) | Design notes for shipped features (subagent context rollover). |
| [incidents/](incidents/) | Incident write-ups: timeline, root cause, fixes. |
| [extensions/](extensions/) | Extension authoring: protocol, hooks, permissions, tutorial. |
| [smoke/](smoke/) | Manual smoke-test procedures. |

New contributors should start with [../AGENTS.md](../AGENTS.md), which is the developer and agent orientation guide for the whole workspace.
