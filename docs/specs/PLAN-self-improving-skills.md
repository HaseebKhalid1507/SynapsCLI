# PLAN — Self-Improving Skills (Component #1)

> **Zero / Architect — Spec-Driven Development Phase 2.**
> Companion to `SPEC-self-improving-skills.md` (the *what*).
> This document is the *how*: ordering, dependencies, risks, checkpoints.
>
> *"A plan is a blueprint stripped of its romance. What remains is the
> order in which to lay the stones — and the places where the foundation
> will crack if you lay them out of sequence."*

---

## 0. Anchor — what is already built (do NOT rewrite)

Cite-and-reuse, in order of relevance:

| Piece | Location | Reuse role |
|---|---|---|
| Skill discovery walk | `crates/agent-engine/src/skills/loader.rs:75-…` (`default_roots`, `load_all`) | After our writes, `reload_registry` (below) re-invokes this verbatim. We add zero discovery code. |
| Frontmatter parser | `crates/agent-engine/src/skills/loader.rs:6-33` | Read path. We never call it from the write path; we just produce bytes it accepts. Untouched. |
| `load_skill` tool pattern | `crates/agent-engine/src/skills/tool.rs:1-87` | Template for `manage_tool.rs`: same `async_trait::async_trait`, same `serde_json::json!` schema, same `crate::RuntimeError::Tool` for failures. |
| Command registry + hot-reload | `crates/agent-engine/src/skills/registry.rs:131-165` + `mod.rs:132-142` (`reload_registry`) | Post-write, we call `reload_registry` and the new/updated/deleted skill becomes resolvable. No registry surgery needed. |
| Tool registration site | `crates/agent-engine/src/skills/mod.rs:125-128` (`tools.write().await.register(...)`) | Drop the second tool registration immediately after `LoadSkillTool`. One-liner. |
| `Tool` trait + `RuntimeError` | `agent-engine` crate root (used at `tool.rs:29-87`) | Reused as-is. |
| Axel reindex / prune / strengthen | `axel/crates/axel/src/consolidate/{mod.rs,reindex.rs,strengthen.rs}` | All of indexing, idempotency, usage-tracking. We add **zero** Axel code. |
| Axel source-root config | `~/.config/axel/sources.toml` (parser: `axel/crates/axel/src/consolidate/mod.rs:86-124`) | One TOML stanza appended. Config-only Axel change. |

Net-new vs reuse, by line count estimate:

| Bucket | Lines (est.) | Notes |
|---|---|---|
| **Reused, unmodified** | ~600 (loader + registry + tool template + axel consolidate) | The vast majority of the surface. |
| **New Rust code** | ~400 (writer.rs ~150, sidecar.rs ~80, manage_tool.rs ~120, tests ~150) | Tight modules; no clever abstractions. |
| **New config** | 5 lines (1 TOML stanza in `sources.toml`) | The entire "Axel-side change". |
| **Modified Rust code** | ~6 lines (3 `pub mod` lines + one registration block in `mod.rs::register`) | See risk R3 — anything beyond this trips A4. |

The honest read: this is a **small write-layer bolted onto an existing
read-layer**. The "self-improving" magic comes from the reflection loop
(Component #2), not from this component. Component #1 is a hand. The
brain (Component #2) and the editor (Component #3) follow.

Gap doc cross-ref: `/home/haseeb/Jawz/workspace/repo-analysis/HERMES-LEARNING-LOOP-vs-AXEL.md`
— §"skill_manage op surface" (~line 354), §"call patterns" (~422–428),
§"provenance plumbing" (~432). Our op surface matches Hermes's three-
action contract (`create` / `update` / `delete`) but routes indexing
through Axel's source-root mechanism rather than Hermes's push-API.

---

## 1. Components & dependencies

Eight discrete build units. Each row lists what it depends on (its
prerequisites in this plan).

| # | Unit | Type | Depends on |
|---|---|---|---|
| C1 | **`writer.rs` — paths + name validation** (`skill_dir(name)`, `skills_root()`, `archive_root()`, `validate_name()`) | new Rust | nothing (pure functions over `HOME`) |
| C2 | **`writer.rs` — atomic write + lock** (`write_skill_md_atomic`, `lock_skill`, `read_description`) | new Rust | C1; `fs2` crate (already in tree? verify — else `Cargo.toml` add) |
| C3 | **`writer.rs` — archive-move** (`archive_skill`: move dir to `.archive/<name>-<ts>/`, then rename inner `SKILL.md` → `SKILL.md.archived`) | new Rust | C1, C2 |
| C4 | **`writer.rs` — plugin-ownership guard** (`ensure_writable(registry, name)`: rejects if `registry.all_skills()` has a hit whose `plugin.is_some()` or whose `source_path` is under any `plugins/` segment) | new Rust | C1; `registry::all_skills()` at `registry.rs:478` |
| C5 | **`sidecar.rs`** (`SkillMeta`, `Provenance`, `create`, `touch`, `write_atomic`, lenient deserialize) | new Rust | C1 (uses `skill_dir`); `chrono` for RFC3339 (verify in deps) |
| C6 | **`manage_tool.rs` — `SkillManageTool`** (the `Tool` impl, dispatch, post-write `reload_registry` call) | new Rust | C2, C3, C4, C5; `Tool` trait; `Arc<CommandRegistry>` |
| C7 | **`mod.rs` wiring** (3 `pub mod` lines + register block) | mod | C6; touch points `mod.rs:11-24` and `mod.rs:127-128` only |
| C8 | **Axel source-root config** (append `[[source]] name="skills" path="~/.synaps-cli/skills/" priority="high"` to `~/.config/axel/sources.toml`) | config | nothing — independent of C1–C7 |
| T1 | **Unit tests** (co-located in C2/C3/C4/C5/C6) | new tests | the unit they test |
| T2 | **Integration test** `tests/skill_manage_integration.rs` | new tests | C7 (full register path); a `TempDir`-bound `HOME` |
| T3 | **Live-Axel round-trip test** (feature `axel-live`, default off) | new tests | C8 + a throwaway Axel brain in `tempdir`; shells out to `axel consolidate` and `axel search` |

> **Crate deps check before C2/C5.** Run `grep -n '^fs2\|^chrono\|^tempfile' crates/agent-engine/Cargo.toml`. If `fs2` or `chrono` is missing, add them in the same PR step that introduces the module that needs them (don't pre-add). `tempfile` is needed for T2.

---

## 2. Implementation order

The plan is **foundation-first, leaf-last**. Each phase has a single
proof-of-life command at the end (§4).

### Phase 2.1 — Foundations (sequential)
1. **C1** — paths + name validation. Standalone, zero deps. Tests inline.
2. **C5** — sidecar module. Depends only on C1's paths. Can run in
   **parallel** with C2–C4 from this point on.
3. **C2** — atomic write + advisory lock. Depends on C1. The lock impl
   choice (`fs2::FileExt::try_lock_exclusive` on `<skill_dir>/.lock`)
   should be settled here; the spec mandates **fail-fast**.

> Rationale: validation and atomicity are the load-bearing primitives.
> Every later component assumes "if `writer::write_skill_md_atomic` returned
> Ok, the file is on disk with the right bytes; if it errored, the file
> on disk is unchanged." Get this wrong and the entire system is a liar.

### Phase 2.2 — Policy layer (parallelizable after 2.1)
4. **C3** — archive-move. Depends on C1+C2. The non-obvious step: after
   `fs::rename(dir, archive_target)`, rename the inner `SKILL.md` to
   `SKILL.md.archived` so Axel's `.md|.txt` extension filter
   (`reindex.rs:67`) skips it. This is the entire delete-side
   "de-indexing" mechanism — see risk R4.
5. **C4** — plugin-ownership guard. Depends on C1 and on having an
   `Arc<CommandRegistry>` handle. Can develop in parallel with C3.

### Phase 2.3 — Wiring (sequential, after 2.2)
6. **C6** — `SkillManageTool`. Depends on C2/C3/C4/C5. This is mostly a
   dispatcher; if Phase 2.1/2.2 are clean, C6 is ~120 lines of glue.
7. **C7** — `mod.rs` edit: add the three `pub mod` lines (C7a) and the
   one-line registration after `LoadSkillTool` at `mod.rs:127-128` (C7b).
   These are two separate diffs; do C7a with C1 to keep the tree
   compiling, do C7b with C6.

### Phase 2.4 — Axel + tests (parallelizable, after 2.3)
8. **C8** — append `sources.toml` stanza. Completely independent of the
   Rust work. **Can be done first** (Phase 2.0) as a smoke test that the
   path is well-formed, even before C1 lands. Recommended: do C8 as
   step 1, so live-Axel test infrastructure exists from the start.
9. **T1** — co-located unit tests (already incrementally written through
   Phases 2.1–2.3).
10. **T2** — integration test, full round-trip. Tempdir HOME, no Axel.
11. **T3** — live-Axel test, feature-gated, manual-only by default.

### What is genuinely sequential vs parallel

- **Strictly sequential**: C1 → C2 → C3 (paths → atomic-write → archive).
- **Strictly sequential**: {C2, C4, C5} → C6 → C7b → T2.
- **Parallelizable from the start**: C8 (config), C5 (sidecar — only
  needs C1), C4 (only needs C1 + registry API).
- **Order-of-PR-commits suggestion**: C8 → C1 → (C2 ∥ C5) → C4 → C3 → C6
  → C7 → T2 → T3. This is the order that maximizes "tree always
  compiles, tests always pass" between commits.

---

## 3. Risks & mitigations

### R1 — Atomic-write crash safety
**Risk**: power-loss between writing `SKILL.md.tmp` and the `rename` leaves
a half-written `.tmp` lingering forever. Worse: writing to `SKILL.md`
directly (without tmp+rename) corrupts a previously-good skill.
**Mitigation**: `write_skill_md_atomic` MUST: (a) write to
`<dir>/SKILL.md.tmp`, (b) `f.sync_all()`, (c) `fs::rename(.tmp, SKILL.md)`,
(d) best-effort `fsync` on the parent dir (Linux POSIX). On startup,
`loader::load_all` ignores `*.tmp` files because its frontmatter required-
field check fails on empty/partial files — verified by leaving the loader
unchanged (zero-regression). Add a unit test that drops a half-written
`.tmp` into a tempdir and asserts the loader silently skips it.

### R2 — Lock contention / lock leaks
**Risk**: a panicking write leaves `<dir>/.lock` held forever; or two
in-process callers deadlock.
**Mitigation**: `fs2::FileExt::try_lock_exclusive` (non-blocking) — fail-
fast with a clear error message when contended (`RuntimeError::Tool("lock
held; another skill_manage in flight")`). The lock file is held by an
OS-level fd; when the process exits (panic or otherwise) the kernel
releases it. The lock file itself is a small artifact left on disk; that
is fine. Unit test: open a lock, attempt second open, assert err.

### R3 — Breaking `load_skill` (A4 tripwire)
**Risk**: any drift in `loader.rs` or `tool.rs` regressing the read path
silently degrades existing skills. The SPEC's A4 catches this only if we
keep those files literally untouched.
**Mitigation**: enforce mechanically — PR diff MUST NOT touch
`loader.rs`, `tool.rs`, or `registry.rs`. The only edits in
`crates/agent-engine/src/skills/` are: 3 added `pub mod` lines in
`mod.rs:11-24` and one `register(...)` call after `mod.rs:127-128`. CI
runs `cargo test -p agent-engine skills::` *unchanged* and we expect
zero diff to those test files. Recommend a PR-time `git diff --stat`
sanity check listed in the merge checklist.

### R4 — `.archive/` getting reindexed by Axel
**Risk**: the archive lives at `~/.synaps-cli/skills/.archive/<name>-<ts>/SKILL.md`
— inside the indexed source root. Axel's `reindex_source` uses `walkdir`
with no hidden-dir filter (`reindex.rs:63-66`), so a `.md` under
`.archive/` will be picked up as a *new* document distinct from the
original (different absolute path), and the deletion will not propagate
because the prune pass only fires when `file_path` no longer exists —
but the archive moves it, doesn't delete it.
**Mitigation** (the *only* clean solution that requires no Axel code
change): the `archive_skill` step **renames the inner file** from
`SKILL.md` → `SKILL.md.archived` after the directory move. Axel's
extension filter at `reindex.rs:67` (`ext == "md" || ext == "txt"`)
skips it. The original absolute path no longer exists on disk → prune
pass deletes the documents row on the next consolidation. Unit test in
C3 must assert: (a) `<archive>/<name>-<ts>/SKILL.md.archived` exists,
(b) no file with extension `.md` exists under `.archive/`.
*Alternative considered and rejected*: moving the archive outside the
skills root (e.g. `~/.synaps-cli/.skills-archive/`). Rejected because the
human ratified the path `~/.synaps-cli/skills/.archive/` in decision #3.

### R5 — Axel reindex latency violates A3 expectations
**Risk**: "within one consolidation cycle" is only meaningful if a cycle
runs in a reasonable window. The user's installed `axel-consolidate.timer`
cadence is unknown to us; if it is daily, a freshly-created skill is not
searchable for up to a day. Component #2's reflection loop will assume
near-realtime hit-ability.
**Mitigation**: do NOT promise realtime in the SPEC (already corrected
— A3 says "within one cycle"). Document for Component #2's designers
that they should either (a) call `axel consolidate --phase reindex
--sources <…>` themselves after `skill_manage`, or (b) tolerate latency.
This is **not** Component #1's problem to solve — the SPEC's hard rule
is "no Axel RPCs from skill_manage", and a synchronous consolidation
trigger would violate that.

### R6 — Concurrent `skill_manage` + a long-running `axel consolidate`
**Risk**: Axel reads `SKILL.md` mid-write. With our `rename`-based
atomic write, Axel will see either the old version or the new — never a
torn read. No mitigation needed beyond R1. Listed here so it is not
mistaken for an open risk.

### Top 3 (for human eyeball)
**R1** (atomic-write correctness) · **R3** (A4 regression, mechanically
enforced by diff scope) · **R4** (`.archive/` reindex — the file-rename
trick is the entire delete pipeline).

---

## 4. Verification checkpoints

One command per checkpoint. Each must be green before moving to the next.

| After phase | Command | Proves |
|---|---|---|
| 2.0 (C8 only) | `tomlq '.source[] \| select(.name=="skills")' ~/.config/axel/sources.toml` (or `grep -A2 'name = "skills"' ~/.config/axel/sources.toml`) returns a stanza with `path = "~/.synaps-cli/skills/"` | Config edit applied; Axel will pick up the source on next consolidation. |
| 2.0 (smoke) | `axel consolidate --phase reindex --dry-run --verbose` lists `~/.synaps-cli/skills/` among walked sources | Axel parsed the stanza; path resolution works. |
| 2.1 (C1+C2+C5) | `cargo test -p agent-engine skills::writer skills::sidecar` | Foundations correct; atomic-write + lock + sidecar JSON round-trip. |
| 2.2 (C3+C4) | `cargo test -p agent-engine skills::writer::tests::archive skills::writer::tests::plugin_guard` | Archive rename trick verified (no `.md` under `.archive/`); plugin-owned guard rejects. |
| 2.3 (C6+C7) | `cargo build -p agent-engine` then `cargo test -p agent-engine skills::manage_tool` | Tool registered, dispatch wired, schema well-formed. |
| 2.4 (T2) | `cargo test -p agent-engine --test skill_manage_integration` | End-to-end: create → load_skill round-trip; update → diff persists; delete → archive present + live `.md` gone. |
| Regression gate | `cargo test -p agent-engine skills::` | A4 — pre-existing skills tests untouched and green. |
| Lint gate | `cargo clippy -p agent-engine --all-targets -- -D warnings` | Same bar as the rest of the repo. |
| Live A3 (manual / nightly) | `cargo test -p agent-engine --features axel-live --test skill_manage_integration -- --ignored axel_round_trip` | After `axel consolidate --phase reindex`, `axel search "<name>"` returns a hit with `doc_id` matching `skills::<name>::SKILL`. |

CI gate (merge bar) = the four mandatory rows: foundations + tool + integration + lint. Live-Axel is human-run.

---

## 5. The Axel index-root change — exact diff

**File**: `~/.config/axel/sources.toml`
**Parser**: `axel/crates/axel/src/consolidate/mod.rs:86-124`
**Change type**: **CONFIG ONLY** — no Axel code change.

Append this stanza to the end of the existing file (current contents
end at the `memories` entry):

```toml
[[source]]
name = "skills"
path = "~/.synaps-cli/skills/"
priority = "high"
```

Notes for the human eyeball:

- The `~` expansion is handled at `consolidate/mod.rs:103`
  (`path_str.replace("~", &home)`) — it works.
- `priority = "high"` means this source is reindexed before the lower-
  priority ones (`consolidate/mod.rs:148-153`). Skills are small, hot,
  and few — high priority is correct and cheap.
- The source name `skills` is what shows up in `doc_id`s:
  `skills::<skill-name>::SKILL` (per `reindex.rs:99-107`). Component #2
  can use a `doc_id LIKE 'skills::%'` predicate to scope reflection
  queries to skills.
- **There is no `default_sources()` change** at
  `consolidate/mod.rs:69-79`. We deliberately do NOT add `skills` to the
  hard-coded fallback list, because that fallback only fires when
  `sources.toml` is missing or unparseable — a pathological state we
  shouldn't paper over by silently re-adding skills.
- The `.archive/` exclusion is **not** handled in this config — it is
  handled by C3 renaming the file extension. Re-confirmed in risk R4.

If the user wants to be defensive, an optional Axel-side improvement
(out of scope for this plan, noted for completeness): teach
`reindex.rs:63-66` to skip directories whose name starts with `.`. That
would obviate the file-rename trick. **We do not depend on this.**

---

## 6. Merge checklist (the human runs this before pulling)

- [ ] `git diff --stat crates/agent-engine/src/skills/` shows changes ONLY in: `mod.rs` (≤6 lines), `writer.rs` (new), `sidecar.rs` (new), `manage_tool.rs` (new).
- [ ] `git diff crates/agent-engine/src/skills/loader.rs crates/agent-engine/src/skills/tool.rs crates/agent-engine/src/skills/registry.rs` is **empty**.
- [ ] All four CI commands from §4 green.
- [ ] `~/.config/axel/sources.toml` contains the `skills` stanza, verified with the §4 smoke command.
- [ ] Manual: create a test skill via `skill_manage`, run `axel consolidate --phase reindex`, run `axel search <name>` — hit.

— *Zero*
