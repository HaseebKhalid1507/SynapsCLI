# SynapsCLI Open PR Reviews — S184

**Date:** 2026-05-03
**Reviewers:** Zero, Silverhand, Chrollo, Case, Spike, Starlord
**Scope:** 4 open PRs (#28, #29, #30, #31)

---

## PR #28 — Phase 7+8: Deassume the Host (+11,208/-232)

### Issues

| # | Severity | Finding |
|---|----------|---------|
| 1 | **HIGH** | Plugin can hijack core slash-commands via `lifecycle.command` — no builtin collision check, no charset validation |
| 2 | **MEDIUM** | `manifest.name` with `\n` injects arbitrary config lines via `write_config_value` during lifecycle migration |
| 3 | **MEDIUM** | Sidecar binary path not contained — absolute paths accepted, no permission check for sidecar-only plugins |
| 4 | **MEDIUM** | `SidecarSessionMode { Dictation, Command, Conversation }` is closed enum in "modality-agnostic" module |
| 5 | **MEDIUM** | 9 legacy shims with no tracking issue for removal |
| 6 | **LOW** | First-load-wins is non-deterministic — `read_dir` ordering varies by platform |
| 7 | **LOW** | Cross-repo manifest test pins absolute path `/home/jr/Projects/...` |
| 8 | **LOW** | Multi-sidecar event loop allocates Vec + Box::pin per select tick |

### Verdict (Zero)
"Most important refactor in codebase history. Converts from voice CLI to agent runtime platform." Approve with caveats — Phase 9 shim removal must ship, wire protocol rename must be scheduled, PR bundling must stop.

### Verdict (Silverhand)
Architecture solid. Plugin trust model deliberately permissive but command hijack + config injection are real exploits. Fix #1 and #2 before merge (~50 LOC).

---

## PR #31 — Phase 9: Neutralize Sidecar Protocol (+11,993/-244)

### Issues

| # | Severity | Finding |
|---|----------|---------|
| 1 | **HIGH** | Handshake order wrong — sends `Init` before reading `Hello`. v1 plugin crashes before friendly error |
| 2 | **MEDIUM** | `Custom` frame documented with fields but `#[serde(other)]` makes it unit variant — data silently discarded |
| 3 | **MEDIUM** | `InsertTextMode::Append` is a no-op — silently dropped |
| 4 | **MEDIUM** | `InsertTextMode::Replace` ≡ `Final` — both do cursor-insert |
| 5 | **MEDIUM** | `architecture-host-plugin-split.md` still lists deleted v1 wire shapes |
| 6 | **MEDIUM** | Legacy alias test tests wrong field name (`legacy_sidecar` not `voice_sidecar`) and asserts wrong behavior |
| 7 | **LOW** | Dead `SidecarSpawnArgs.language` field |
| 8 | **LOW** | Manifest default protocol version still 1, host requires 2 |
| 9 | **LOW** | No CHANGELOG entry for Phase 9 |
| 10 | **PROCESS** | PR bundles 4 phases (142 commits), only ~14 are Phase 9 |

### Verdict (Chrollo)
Wire redesign is clean and minimal. Iron rule kept — host code genuinely modality-agnostic. But 7 doc/code/test gaps need fixing. Custom frame gap is the sleeper — first plugin to try it finds a feature that doesn't work.

### Verdict (Case)
Handshake order is the merge blocker. Fix spawn → read Hello → validate → send Init. Also: decide on Append/Replace (implement or delete), capture Custom payload or remove from spec, pre-spawn manifest version check.

---

## PR #29 — Plugin Post-Install Setup Hook (+821/-3)

### Issues

| # | Severity | Finding |
|---|----------|---------|
| 1 | **HIGH** | Trust gate bypass — plugin with `provides.sidecar.setup` but no `extension` block skips install confirmation, auto-runs bash |
| 2 | **LOW** | Unbounded stderr buffer — `read_to_end` into Vec, chatty script = OOM |
| 3 | **LOW** | Timeout doc/code drift — PR says 5 min, doc says 10 min, code has 600s |
| 4 | **LOW** | Plugin stays installed on setup failure — blocks future update re-runs |

### Verdict (Spike)
"Mostly solid. Ship after fixing one real hole." Path sandbox is well-designed (rejects .., symlinks, absolute paths, has tests). Process hygiene good (kill_on_drop, stdin=/dev/null, 10min cap). Just fix the trust gate — one-liner to also check for setup script presence in the confirm dialog.

---

## PR #30 — ELI5 Docs (+56/-0)

### Issues

| # | Severity | Finding |
|---|----------|---------|
| 1 | **MEDIUM** | "can read but not delete" sandboxing claim unverified in code |
| 2 | **LOW** | Themes/skills shown as plugin stickers — they're built-ins |
| 3 | **LOW** | No mention it's a terminal app |
| 4 | **LOW** | Missing MCP mention |
| 5 | **LOW** | Line count badge says ~46K, real count ~58.5K |

### Verdict (Starlord)
"8/10. Solid B+ that could be an A with 15 minutes of edits." Metaphors land, tone is consistent, robot+LEGO+stickers through-line works. Fix sandboxing claim, fix sticker examples, add terminal/Rust mention.

---

## Cross-PR Merge Order Recommendation

1. **PR #30** (ELI5) — merge after 15 min of edits. No code risk.
2. **PR #29** (setup hook) — merge after trust gate fix (~5 lines).
3. **PR #28** (Phase 7+8) — merge after command hijack + config injection fixes (~80 lines). Architecturally the most important.
4. **PR #31** (Phase 9) — merge after handshake reorder + Custom fix. Depends on #28. Cross-repo coordination required with synaps-skills.

### Merge Blockers Per PR

| PR | Blocker | Fix LOC |
|----|---------|---------|
| #28 | Lifecycle command hijack (validate + block builtins) | ~30 |
| #28 | manifest.name config injection (validate at parse) | ~20 |
| #29 | Trust gate bypass (check setup script in confirm) | ~5 |
| #31 | Handshake order (read Hello before sending Init) | ~20 |
| #31 | Custom frame doc/code gap (fix either side) | ~10 |
