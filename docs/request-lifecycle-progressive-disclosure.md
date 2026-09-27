# Progressive tool disclosure (opt-in)

Task 18 adds an experimental `progressive_tool_disclosure` configuration flag.
It defaults to `false`; the default request path continues to use the registry's
existing cached full-schema `Arc`, so enabling this work does not change request
bytes unless the user opts in.

```text
progressive_tool_disclosure = true
```

## First-request core

When enabled, a new stream exposes only cataloged members of this deterministic
local core:

- local operations: `bash`, `read`, `write`, `edit`, `grep`, `find`, `ls`;
- tool discovery/authorization: `search_tools`, `activate_tools`;
- skill discovery/loading when registered: `search_skills`, `load_skill`.

Missing or explicitly disabled core members are not invented. Specialized
subagent lifecycle tools, stateful shell tools, extension tools, MCP tools, and
other dynamically registered capabilities remain cataloged but absent from the
request until an exact session activation is authorized. Subagent registries do
not gain the activation gateways.

Skill *tool schemas* are handled here. Lazy reading of dormant skill bodies is a
separate Task 21 concern; this flag does not claim that boot is body-lazy yet.

## Byte budget

The first-request budget is **8 KiB** (8,192 bytes), enforced by the automated
Task 18 fixture on two metrics at once:

- the compact serialized JSON array of the first-request tool schemas;
- the Task 12 canonical `trace::diagnostics::tools_prefix_bytes` for the same
  projection.

The budget is sized against the measured production core, not synthetic
fixtures: `ToolRegistry::new()` plus `progressive_core_for_catalog` projects
9 tools at **4,402 serialized bytes** and **2,453 tools-prefix bytes**
(measured at fix time; the test re-measures on every run). The fixture also
measures 10, 100, 500, 1,000, and 2,000 dormant catalog entries and requires
byte-identical first-request schemas — on both metrics — at every size:
**first-request bytes are invariant in dormant tool count.** The projection
selects already-cached schema values; catalog insertion itself does not
rebuild or expose a session projection.

Known deferral (Task 21): when skills are registered, the `load_skill`
gateway's schema currently inlines the installed skill index (one
`name — description` line per skill), so flag-on first-request bytes still
grow with the number of installed *skills* — not with dormant *tool* count.
Lazy skill-body/index disclosure is Task 21's scope; until then the budget is
guaranteed only for the tool catalog dimension, and the production-core test
pins the skill-free baseline.

Model-initiated `activate_tools` is gated by `tools.activation_confirm`
(`auto` — default, granted without a prompt; `prompt` — the host is asked with
a y/n "Confirm tool activation" dialog listing the exact ids, only `y`/`yes`
allows; `deny` — always refused, no prompt). `server.auto_approve_confirms`
grants regardless of the key.

A successful `activate_tools` batch advances the session schema generation once.
The next provider round recomputes one provider-neutral projection from that
same retained `SessionToolSet`, so all transports receive the same logical
active tools and dormant siblings remain absent. Catalog generation drift
invalidates prior activations and rebuilds the stream with the same minimal-core
policy.
