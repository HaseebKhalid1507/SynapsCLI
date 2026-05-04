# Plan: Systematic Hardening of Extension Install/Post-Install Flow

**Repo:** `HaseebKhalid1507/SynapsCLI`  
**Branch/worktree context:** current analysis on `feat/extension-setup-script`  
**Target area:** extension install, update, setup, prebuilt download/extraction, verification, progress UI, registry state  
**Created:** 2026-05-03

---

## 1. Purpose

This plan converts the three review reports — code review, systematic debugging, and security review — into an ordered implementation roadmap. The goal is to make SynapsCLI extension install/post-install behavior safe, debuggable, and predictable before broad rollout of:

- `extension.setup`
- `extension.prebuilt`
- skip-if-exists extension binary verification
- post-setup verification
- plugin install progress UI

---

## 2. Non-Goals

- Do not redesign the whole plugin marketplace model.
- Do not change plugin manifest semantics beyond the fields needed for secure extension install/post-install.
- Do not ship platform-specific Windows setup support unless explicitly scoped later; this plan only requires clear failure behavior on unsupported setup platforms.
- Do not solve extension runtime sandboxing; this plan focuses on install/post-install.

---

## 3. Convergence Mode

**convergence: informed**

Rationale: this touches security-sensitive plugin installation code, archive extraction, untrusted network I/O, and install-state persistence. It warrants multi-perspective review, but human review is still expected before merge, so full holdout mode is optional.

Fixed parameters:

- `threshold`: `0.8`
- `axis_weights`:
  - correctness: `0.30`
  - security: `0.30`
  - reliability/debuggability: `0.20`
  - maintainability: `0.10`
  - UX/performance: `0.10`
- `max_fix_iterations`: `2`
- `max_total_calls`: `10`

Implementation note: do not adjust these after seeing review scores.

---

## 4. Dependency Graph

```text
Validation / policy primitives
    ├── URL policy
    ├── path/name sanitization
    ├── command path verification
    └── archive entry sandboxing

Prebuilt pipeline
    ├── bounded streaming download
    ├── checksum verification
    ├── safe extraction into temp dir
    └── post-extract extension.command verification

Setup pipeline
    ├── setup command platform policy
    ├── setup environment policy
    ├── setup execution/result typing
    └── setup logs and error hints

Install/update orchestration
    ├── install flow
    ├── update flow
    ├── registry persistence semantics
    └── extension loader skip/disable semantics

UI/progress
    ├── typed install phases
    ├── pending setup/prebuilt task
    └── user-visible retry/failure state

Tests/docs
    ├── unit tests for primitives
    ├── integration tests for install/update
    ├── security regression tests
    └── docs/trust messaging
```

---

## 5. Worktree Requirement

Before implementation begins, create a new dedicated worktree. Do not implement directly in the primary checkout.

Recommended:

```bash
cd /home/jr/Projects/Maha-Media/SynapsCLI
git fetch origin --prune
git worktree add -b feat/extension-install-hardening \
  /home/jr/Projects/Maha-Media/.worktrees/SynapsCLI-extension-install-hardening \
  feat/extension-setup-script
cd /home/jr/Projects/Maha-Media/.worktrees/SynapsCLI-extension-install-hardening
```

If `feat/extension-setup-script` must remain local-only, keep this branch local unless explicitly approved.

---

## 6. Implementation Tasks

## Task 1: Add install security policy helpers

**Description:** Centralize small policy helpers used by later tasks: safe plugin/log filename fragments, allowed prebuilt URL schemes, SHA-256 shape normalization, and maximum prebuilt archive size constants.

**Acceptance criteria:**

- [ ] Production prebuilt URLs allow `https://` only.
- [ ] `file://` prebuilt URLs are allowed only under `#[cfg(test)]` or an explicitly named non-default compile feature.
- [ ] SHA-256 values are validated as exactly 64 lowercase hex chars, or normalized and compared using a documented policy.
- [ ] `install_log_path` no longer trusts raw `plugin_name` as a path component.

**Verification:**

- [ ] Unit tests cover allowed/rejected URL schemes.
- [ ] Unit tests cover SHA-256 validation.
- [ ] Unit tests cover plugin names containing `/`, `\\`, `..`, control chars, and normal safe names.
- [ ] `cargo test -q post_install -- --test-threads=8`

**Dependencies:** None  
**Files likely touched:**

- `src/skills/post_install.rs`
- possibly `src/skills/manifest.rs` or a new small helper module

**Scope:** S

---

## Task 2: Reject absolute `extension.command` paths

**Description:** Tighten `verify_extension_command()` so plugin-shipped extension commands must be relative paths contained inside `plugin_dir`. Bare command names may remain PATH lookups if that is still an intentional supported mode, but absolute paths should be rejected.

**Acceptance criteria:**

- [ ] Absolute `extension.command` returns `CommandVerifyError::EscapesPluginDir` or equivalent.
- [ ] Relative paths with `..` are rejected.
- [ ] Symlink escapes remain rejected.
- [ ] Bare command behavior is explicitly tested and documented.

**Verification:**

- [ ] Existing command verification tests pass.
- [ ] New test rejects `/tmp/ext` or platform equivalent.
- [ ] New test rejects Windows-style absolute path if feasible.
- [ ] `cargo test -q verify_extension_command -- --test-threads=8`

**Dependencies:** Task 1  
**Files likely touched:**

- `src/skills/post_install.rs`
- possibly `src/extensions/manifest.rs`

**Scope:** XS

---

## Task 3: Replace full-buffer prebuilt download with bounded streaming download

**Description:** Rewrite the HTTP prebuilt download path to stream into a temp file while hashing, enforce a maximum archive size, and configure request timeouts.

**Acceptance criteria:**

- [ ] Prebuilt downloads use a `reqwest::Client` with connect and total timeout.
- [ ] Download rejects responses whose `Content-Length` exceeds the configured max.
- [ ] Download rejects streams that exceed max bytes even without `Content-Length`.
- [ ] SHA-256 is computed during streaming.
- [ ] Partial temp file is deleted on checksum mismatch, timeout, or size-limit failure.
- [ ] The implementation does not hold the whole archive in memory.

**Verification:**

- [ ] Unit or integration test for checksum success.
- [ ] Test for checksum mismatch cleans temp file.
- [ ] Test for oversized `Content-Length` fails before extraction.
- [ ] Test for no `Content-Length` but oversized stream fails.
- [ ] Test for stalled/timeout server if local HTTP fixture exists; otherwise isolate client timeout helper.
- [ ] `cargo test -q prebuilt -- --test-threads=8`

**Dependencies:** Task 1  
**Files likely touched:**

- `src/skills/post_install.rs`
- test helpers in same file or integration tests

**Scope:** M

---

## Task 4: Implement safe prebuilt archive extraction sandbox

**Description:** Replace direct `tar`/`unzip` extraction into `plugin_dir` with a safe extraction pipeline. Prefer Rust-native archive handling. If system tools remain temporarily, extract into a temp dir and verify every extracted path stays inside the temp dir before moving validated content.

**Acceptance criteria:**

- [ ] Archive entries with absolute paths are rejected.
- [ ] Archive entries containing `..` are rejected.
- [ ] Symlink/hardlink entries that escape extraction root are rejected or unsupported.
- [ ] Extraction occurs into a dedicated temp extraction directory.
- [ ] The final `extension.command` is verified after extraction and before success.
- [ ] `unzip -o` or equivalent unconditional overwrite behavior is removed.
- [ ] Extraction does not block the async executor; blocking work uses `spawn_blocking` or async process handling.

**Verification:**

- [ ] Tests for tar path traversal.
- [ ] Tests for zip path traversal.
- [ ] Tests for absolute entry paths.
- [ ] Tests for symlink escape.
- [ ] Tests for archive with top-level wrapper directory has documented behavior, either supported via configured strip or rejected clearly.
- [ ] `cargo test -q prebuilt -- --test-threads=8`

**Dependencies:** Task 3  
**Files likely touched:**

- `src/skills/post_install.rs`
- `Cargo.toml` / `Cargo.lock` if Rust archive crates are added

**Scope:** M

---

## Checkpoint A: Prebuilt security foundation

After Tasks 1-4:

- [ ] Focused post-install/prebuilt tests pass with `--test-threads=8`.
- [ ] `cargo build` succeeds.
- [ ] Security review confirms no unbounded download and no archive traversal.
- [ ] No production `file://` escape hatch remains.

---

## Task 5: Introduce typed post-install outcome and severity

**Description:** Replace ambiguous `Result<Option<PathBuf>, String>` post-install result handling with a typed outcome that distinguishes no-op, prebuilt installed, setup succeeded, soft setup failure, and hard security/verification failure.

**Acceptance criteria:**

- [ ] `run_post_install_setup_for_dir` returns a typed enum or structured error.
- [ ] Security failures such as path escape, checksum mismatch, unsafe URL, and invalid command are hard failures.
- [ ] Build/setup script nonzero or timeout is explicitly classified.
- [ ] Callers do not stringify all failures before deciding persistence behavior.

**Verification:**

- [ ] Unit tests cover every outcome variant.
- [ ] Existing setup tests still pass.
- [ ] `cargo test -q post_install -- --test-threads=8`

**Dependencies:** Tasks 1-4  
**Files likely touched:**

- `src/skills/post_install.rs`
- `src/chatui/plugins/actions.rs`

**Scope:** M

---

## Task 6: Define and implement failed-setup registry semantics

**Description:** Decide and encode how Synaps records plugins whose setup/prebuilt/post-verify failed. Recommended model: keep plugin installed only if explicitly recoverable, mark setup status, and prevent extension spawn until setup succeeds.

**Acceptance criteria:**

- [ ] `plugins.json` can represent setup status, or a parallel status mechanism exists.
- [ ] A plugin with failed setup does not attempt extension spawn on session start.
- [ ] UI surfaces setup failure with log path and retry guidance.
- [ ] Hard security failures do not silently record a trusted installed extension.
- [ ] Existing plugins without setup status remain backward-compatible.

**Verification:**

- [ ] Test: setup failure records expected status or does not record install, per chosen policy.
- [ ] Test: failed setup extension is skipped by extension loader.
- [ ] Test: setup success clears failure status.
- [ ] Manual smoke: install plugin with failing setup and restart Synaps; no OS error 2 spawn spam.

**Dependencies:** Task 5  
**Files likely touched:**

- `src/chatui/plugins/actions.rs`
- `src/skills/registry.rs` or plugin state types
- `src/extensions/manager.rs`
- state serialization tests

**Scope:** M

---

## Task 7: Run post-install on update path

**Description:** Mirror install behavior in update confirmation: after the update temp dir is finalized, run prebuilt/setup/post-verify before registry reload completes.

**Acceptance criteria:**

- [ ] `apply_confirm_pending_update` invokes the same typed post-install pipeline as install.
- [ ] Update with source-built extension rebuilds or verifies the binary.
- [ ] Update with prebuilt extension downloads/extracts/verifies as appropriate.
- [ ] Update failure follows the registry semantics from Task 6.

**Verification:**

- [ ] Test update with setup script writes marker/binary.
- [ ] Test update with setup failure preserves/report status according to policy.
- [ ] Test update with changed `extension.command` fails clearly if missing.
- [ ] `cargo test -q update -- --test-threads=8`

**Dependencies:** Tasks 5-6  
**Files likely touched:**

- `src/chatui/plugins/actions.rs`
- plugin management tests

**Scope:** M

---

## Checkpoint B: Correct install/update semantics

After Tasks 5-7:

- [ ] Fresh install and update both run the same post-install chain.
- [ ] Failed setup no longer causes later opaque extension spawn failures.
- [ ] Focused install/update tests pass.
- [ ] Manual smoke with `axel-memory-manager` succeeds from clean install.

---

## Task 8: Add setup/prebuilt progress phases and background task orchestration

**Description:** Extend the current background install model so clone, prebuilt download, extraction, setup, and verification are all represented in progress state and do not freeze the UI.

**Acceptance criteria:**

- [ ] `ClonePhase::SetupRunning` is used or replaced with a more complete `InstallPhase` enum.
- [ ] Prebuilt download/extract phases are represented in UI state.
- [ ] Long setup scripts do not block input/render loop.
- [ ] The install overlay remains visible until post-install is complete or failed.
- [ ] Progress state does not transition backward incorrectly.

**Verification:**

- [ ] Unit tests for phase ordering.
- [ ] Test that `set_setup_running` or equivalent is called in production path.
- [ ] Manual install of plugin with `sleep 5` setup shows setup phase and responsive UI.
- [ ] `cargo test -q plugins -- --test-threads=8`

**Dependencies:** Tasks 5-7  
**Files likely touched:**

- `src/chatui/plugins/progress.rs`
- `src/chatui/plugins/state.rs`
- `src/chatui/plugins/actions.rs`
- `src/chatui/plugins/draw.rs`
- `src/chatui/mod.rs`

**Scope:** M

---

## Task 9: Replace display-string trust gate with typed manifest inspection

**Description:** Stop deciding whether an executable-extension confirmation is needed by matching human-readable summary lines.

**Acceptance criteria:**

- [ ] Confirmation decision uses `PluginManifest::extension.is_some()` or equivalent typed data.
- [ ] Human-readable summary remains purely presentational.
- [ ] If manifest inspection fails, default is conservative: require confirmation or fail install.

**Verification:**

- [ ] Test still prompts for executable extension when summary text changes.
- [ ] Test manifest inspection failure does not bypass trust.
- [ ] Existing trust prompt tests pass.

**Dependencies:** None, but safer after Task 6 if status types change  
**Files likely touched:**

- `src/chatui/plugins/actions.rs`
- `src/skills/trust.rs` if helper is added

**Scope:** S

---

## Task 10: Harden extension load hints and setup path display

**Description:** Ensure extension load failure hints do not embed raw manifest-controlled strings as shell commands. Update hint logic to support `extension.setup` as well as legacy sidecar setup.

**Acceptance criteria:**

- [ ] Hints consider `extension.setup` before `provides.sidecar.setup`.
- [ ] Hints do not include unescaped shell metacharacter-bearing manifest strings.
- [ ] Hints are actionable without encouraging unsafe copy-paste.
- [ ] Terminal control characters are stripped or escaped in user-visible manifest-derived strings.

**Verification:**

- [ ] Unit tests for extension setup hint.
- [ ] Unit tests for malicious setup string containing `; rm -rf` or ANSI escape.
- [ ] Existing manager hint tests pass.

**Dependencies:** Task 1  
**Files likely touched:**

- `src/extensions/manager.rs`
- possibly a string sanitization helper

**Scope:** S

---

## Checkpoint C: UX and trust hardening

After Tasks 8-10:

- [ ] Install UI remains responsive through clone, prebuilt, setup, verify.
- [ ] Executable extension confirmation is typed and conservative.
- [ ] Load hints are safe and include `extension.setup`.
- [ ] Manual smoke install of `axel-memory-manager` shows sensible progress.

---

## Task 11: Clarify setup environment and platform policy

**Description:** Make setup script execution policy explicit: environment inheritance, platform support, shell choice, and error messages.

**Acceptance criteria:**

- [ ] On unsupported platforms, setup fails early with a clear error.
- [ ] If setup uses `bash`, absence of bash is reported clearly.
- [ ] Either setup env is reduced via `.env_clear()` + allowlist, or docs/trust prompt explicitly warn that setup inherits environment.
- [ ] Windows behavior is documented and tested with cfg-gated tests where possible.

**Verification:**

- [ ] Unit test for missing shell or unsupported setup path, if mockable.
- [ ] Docs updated.
- [ ] Manual check error message for nonexistent shell where feasible.

**Dependencies:** Task 5  
**Files likely touched:**

- `src/skills/post_install.rs`
- `docs/plugin-setup-scripts.md`

**Scope:** S

---

## Task 12: Improve temp-dir and stale artifact cleanup

**Description:** Make cleanup failures explicit and recoverable for pending install dirs, temp archives, and extraction dirs.

**Acceptance criteria:**

- [ ] Stale prebuilt temp archive removal errors are surfaced with context.
- [ ] Failed `finalize_pending_install` does not silently discard cleanup errors.
- [ ] Partial final dirs are detected separately from complete installed plugins.
- [ ] Failed temp/extraction dirs are cleaned or quarantined with user-visible path.

**Verification:**

- [ ] Tests simulate final dir already exists and verify temp cleanup/quarantine behavior.
- [ ] Tests simulate stale temp archive and verify contextual error.
- [ ] No temp archive remains after checksum mismatch, timeout, or size-limit failure.

**Dependencies:** Tasks 3-4  
**Files likely touched:**

- `src/chatui/plugins/actions.rs`
- `src/skills/post_install.rs`

**Scope:** S

---

## Task 13: Fix test isolation and timing flakiness

**Description:** Remove known env-var and real-time sleep flakiness in tests introduced by install progress/prebuilt work.

**Acceptance criteria:**

- [ ] All tests that mutate environment variables use shared locks or scoped test-only APIs.
- [ ] `SYNAPS_INSTALL_MIN_DISPLAY_MS` tests are deterministic, preferably using paused Tokio time or direct injection.
- [ ] Base-dir/env locks are shared across modules where needed.
- [ ] Tests pass repeatedly with `--test-threads=8`.

**Verification:**

- [ ] `for i in {1..10}; do cargo test -q post_install -- --test-threads=8 || break; done`
- [ ] `cargo test -q plugins -- --test-threads=8`
- [ ] No new global env races are introduced.

**Dependencies:** Can run anytime, but easiest after Task 8  
**Files likely touched:**

- `src/chatui/plugins/state.rs`
- `src/chatui/plugins/actions.rs` tests
- `src/skills/post_install.rs` tests
- possibly shared test helper module

**Scope:** S

---

## Task 14: Documentation update for plugin authors and users

**Description:** Document the hardened install/post-install contract for extension authors and marketplace users.

**Acceptance criteria:**

- [ ] `extension.setup` precedence over sidecar setup documented.
- [ ] Prebuilt archive layout requirements documented.
- [ ] SHA-256 and HTTPS requirements documented.
- [ ] Setup failure behavior documented.
- [ ] Environment/platform policy documented.
- [ ] Security warnings are clear but not alarmist.

**Verification:**

- [ ] Docs mention all manifest fields: `extension.setup`, `extension.prebuilt`, `url`, `sha256`.
- [ ] Docs include a minimal working example.
- [ ] Docs include troubleshooting for missing binary / failed setup.

**Dependencies:** Tasks 1-12  
**Files likely touched:**

- `docs/plugin-setup-scripts.md`
- `docs/extensions/*` or new doc if appropriate

**Scope:** S

---

## Final Checkpoint: Release readiness

After all tasks:

- [ ] `cargo fmt --check`
- [ ] `cargo build`
- [ ] `cargo test -- --test-threads=8`
- [ ] Known pre-existing flaky tests documented separately if still present.
- [ ] Manual clean install of `axel-memory-manager` succeeds.
- [ ] Manual update/reinstall of `axel-memory-manager` succeeds.
- [ ] Manual failing setup plugin does not cause opaque extension spawn failure on restart.
- [ ] Security review rerun on final diff.
- [ ] Code review rerun on final diff.
- [ ] PR description includes migration/backward-compat notes.

---

## 7. Suggested Commit Slices

1. `sec(post-install): centralize install policy validation`
2. `sec(extensions): reject absolute extension commands`
3. `feat(post-install): stream and cap prebuilt downloads`
4. `sec(post-install): sandbox prebuilt archive extraction`
5. `refactor(post-install): return typed setup outcomes`
6. `feat(plugins): persist setup status and skip failed extensions`
7. `fix(plugins): run post-install setup during updates`
8. `feat(plugins): show setup/prebuilt install progress phases`
9. `sec(plugins): use typed extension manifest trust gate`
10. `sec(extensions): sanitize setup hints`
11. `docs(extensions): document setup/prebuilt install contract`
12. `test(plugins): harden install env/time isolation`

---

## 8. Open Questions

1. Should failed setup keep the plugin installed with `setup_failed` status, or roll back install entirely?
   - Recommendation: keep installed but disable extension load until setup succeeds.

2. Should bare PATH commands be allowed for extensions?
   - Recommendation: allow only if explicitly intended; otherwise require plugin-relative commands for extension binaries.

3. Should local `file://` prebuilt support exist outside tests?
   - Recommendation: no, not in production builds.

4. Should prebuilt archives support top-level wrapper directories?
   - Recommendation: either document flat layout only, or add a manifest field like `strip_components`. Avoid guessing.

5. Should setup scripts run with a sanitized environment by default?
   - Recommendation: yes, eventually. If compatibility risk is high, document first and add env isolation in a follow-up.
