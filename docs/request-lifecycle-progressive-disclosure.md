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

The serialized compact JSON array of the first-request tool schemas has a
**4 KiB maximum** in the automated Task 18 fixture. The fixture measures 10,
100, 500, 1,000, and 2,000 dormant catalog entries and requires byte-identical
first-request schemas at every size. The projection selects already-cached
schema values; catalog insertion itself does not rebuild or expose a session
projection.

A successful `activate_tools` batch advances the session schema generation once.
The next provider round recomputes one provider-neutral projection from that
same retained `SessionToolSet`, so all transports receive the same logical
active tools and dormant siblings remain absent. Catalog generation drift
invalidates prior activations and rebuilds the stream with the same minimal-core
policy.
