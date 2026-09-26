# Context continuation follow-up: Axel compatibility boundary

Historical audit; implementation has since landed in the worktree. See
[unified backend](../specs/axel-host-backend.md) for current behavior.

Status at audit time: source audit during checkpoint resume from `c12d603e`; **not an
implemented adapter or migration**. The stored checkpoint accurately identified
unfinished Axel work. No plugin process was started, private brain/session data
read, external checkout changed, or memories imported/deleted for this audit.

## Current routes are separate

- `crates/agent-engine/src/tools/memory.rs` calls the host JSONL note store
  directly. Its `history`/`ctx-` branches use context archives.
- `extensions/runtime/process.rs` exposes legacy extension `memory.append/query`
  against extension namespaces, not the Axel project service.
- Continuous recall/capture in `runtime/mod.rs` uses exact extension tool leases.
- Installed Axel tools own a separate brain. Tool aliases alone cannot unify
  storage or establish one authority.

## Rechecked blockers

Installed plugin source was found through the documented plugin root and the
`axel-memory-manager` name. It advertises 0.3.0 but pins Axel dependencies to
`562e6508f5de0cdc0bbc803b2448aeb7431a6bed`. Its build documentation says that pin
lacks APIs imported by recall and requires a later patched local worktree. The
worktree named by that documentation was absent. This audit therefore does not
certify a replacement Axel revision or installed binary provenance.

1. **Dispatch:** host `runtime/memory_context.rs` requires declared tools named
   `memory_recall` / `memory_capture`. The plugin declares only the four CRUD
   tools and implements `context_provider.recall/capture` as separate RPC
   methods. Exact tool dispatch sends `tool.call`. Fixture tests accept both
   conventions and cannot establish compatibility with this plugin.
2. **Scope:** host `memory/store.rs` derives `p…` from canonical OS path bytes;
   plugin `scope.rs` derives `proj_…` from lossy UTF-8 bytes. The prefix differs
   even for UTF-8 roots, and prefix substitution fails for non-UTF-8 paths.
3. **Capture:** host sends structured `terminal_turn/1`/summary evidence with
   `project_id`, source digest and user/assistant/tool fields. Plugin's strict
   payload expects `project_key`, flattened content and optional source IDs.
   Unsupported fields are rejected. Scope checking and project-qualified commit
   queries must be part of the agreed capture contract, not assumed.
4. **CRUD policy:** host batch fetch, literal substring search, `sensitive`
   classification and retention-days semantics differ from plugin single-ID
   fetch, lexical search, two sensitivity classes, disclosure classes,
   hour-based expiry and minimum content length. Do not silently weaken them.
5. **Commit results:** unknown/malformed capture acknowledgement must not be
   interpreted as absent and blindly retried. Require typed identity-matching
   acknowledgements with explicit possibly-committed outcomes.

## Next safe implementation contract

Introduce an immutable host-owned backend binding, bounded typed CRUD operations
and a legacy adapter preserving current behavior. An explicitly selected Axel
backend that is missing or incompatible must refuse, never open/fall back to the
legacy store. Route all note entry points through that binding or explicitly
refuse them in Axel mode, including extension reverse-memory calls and namespaced
CRUD aliases. Preserve worker permission restrictions. Leave archive artifacts
separate from claims of completed note unification.

Before a real Axel adapter can be enabled:

- Obtain and record one reproducibly buildable, approved backend revision.
- Negotiate and pin operation schemas rather than infer compatibility from a
  manifest version. Keep host-selected scope/storage binding authoritative.
- Preserve old IDs and scope aliases, absolute expiry, provenance, sensitivity,
  disclosure and deletion evidence via an explicit reversible migration plan.
- Do not import at startup, tombstone legacy memories on rollover, or duplicate
  writes during failure/retry. Exact-ID forgetting is not content-wide erasure.
- Prove zero fallback using a poisoned legacy adapter and failing fake Axel.
- Test the real exact backend binary in temporary storage, not only permissive
  fixtures: dispatch/schema agreement, non-UTF-8 scope, secret non-disclosure,
  expiry/tombstone preservation, kill-after-commit and unknown-commit behavior.

Relevant existing tests: `memory_context_e2e`, `continuous_memory_adversarial`,
`memory_history_import`, `extension_lease_lifecycle`, `deferred_host_context`,
`memory_project_scope`, `memory_index`, `disclosure_retention`, and
`extensions_memory`. Historical review gates in
`docs/reviews/continuous-memory-cp-f.md` concern recorded local revisions, not
proof of today's installed pairing. The earlier no-SQLite decision in
`docs/decisions/T33-memory-index-no-sqlite.md` also rules out casually adding a
new host database as a shortcut.
