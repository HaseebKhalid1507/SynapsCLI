# Multimodal attachment validation — 2026-09-05

## Result

Implemented local inline request attachments for TUI, headless chat and stdio RPC.
Usage and support boundaries: [Local attachments](../multimodal.md).
All pre-existing uncommitted changes were preserved; no commit or PATH installation
was performed. Installed binary remains SHA-256
`cc554f71b44d22fe7bd7f1da8a4ce2b05ac833488966fb1ad90e6e8556ca6136`.

## Verification

All commands used at most eight compilation workers; test harnesses used one thread.

- `cargo test --workspace --locked --offline -j8 --no-fail-fast -- --test-threads=1`:
  **4,071 passed, 0 failed, 34 ignored** across 122 reported suites; exit 0.
- `cargo clippy --workspace --lib --bins --locked --offline -j8 -- -D warnings`:
  **passed**, exit 0.
- Initial `cargo check --workspace --all-targets --locked --offline -j8`: passed.
- `git diff --check`: passed.
- All-target strict Clippy is not green: existing, otherwise untouched tests report
  `cloned_ref_to_slice_refs` in `crates/agent-core/tests/provider_confinement.rs:65`,
  `type_complexity` in `crates/agent-core/src/core/auth/github_copilot.rs:967,969`,
  and `assertions_on_constants` in that file at line 1111.

Local logs: `/tmp/synaps-multimodal-check.sYM2oL/`:
`tests-final.log`, `tests-final.exit`, `clippy-prod.log`, `clippy-prod.exit`,
`clippy.log`, `clippy.exit`, `check.log`, `check.exit`.

Tests cover bounded regular-file loading, image/document source validation,
exact capability evidence, Chat/Responses wire formats, sibling tool-media
ordering, aggregate overflow recovery, zero-send preflight rejection, pending
submission atomicity, private session round trips, archive/capture exclusion,
restored display and compaction projections. No paid live model inference smoke
test was performed; model response correctness is not claimed from wire tests.

## Review findings addressed

1. Individually valid sibling tool media could exceed aggregate limits and poison
   saved history. Both streaming and nonstreaming now validate the proposed batch
   and replace overflowing new rich outputs with explicit text errors, preserving
   correlation IDs and result order.
2. Legacy image pruning preceded validation. Original outgoing history now passes
   validation first; oversized/malformed media cannot disappear before the gate.
3. OpenAI text-document lowering removed structural provenance before optional
   content capture. Such requests now visibly withhold the content-capture bundle
   using canonical provenance. Metadata tracing continues; structured binary media
   is scrubbed from permitted content captures.

Two environment-dependent test fixtures were made deterministic: Copilot catalog
fallback no longer depends on a developer's live account, and the independent
extension-memory fixture explicitly selects its legacy binding instead of inheriting
the operator's host backend. Production configuration was not changed.

## Boundaries

Astra's observed exact Codex catalog advertises text and image, not PDF/file input.
Images are actual `input_image` parts; UTF-8 attachments lower to `input_text`.
PDF acceptance requires supported native Anthropic or advertised compatible file
capability. Unsupported routes fail closed. This is inline attachment input, not
an external Files API lifecycle, image clipboard decoder or WebSocket binary
upload implementation. Submitted attachment bytes persist in private sessions;
canonical image/document source bytes are excluded from archive-memory capture.
