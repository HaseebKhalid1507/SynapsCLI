# CP-D — Existing-history import (informed checkpoint)

- **Plan:** `axel-continuous-memory-plan/1`, Phase D (Tasks D1–D4)
- **Spec:** `axel-continuous-memory/1`
- **Worktree:** host `SynapsCLI-continuous-memory` @ `8eb2beea`

## Tasks delivered

| Task | Commit | Summary |
|---|---|---|
| D1 | `cfd89787` | History-import disclosure preview + consent gate. Host-computed preview (project id+root, session count, approx bytes, date range, included/excluded classes, retention/redaction, destination `.r8` path, confirmation requirement) via one engine implementation. Model tool call is a proposal only and cannot self-confirm (UserIntentProof). Confirmed consent returns a typed `ImportPlan`; import does not begin here. |
| D2 | `540782ef` | Host-mediated session streaming. Consented `ImportPlan` streams sessions through the canonical host session API (legacy JSON **and** journal via one API), applies disclosure/retention/redaction, builds bounded C1-shaped batches, sends via the C3 capture path. Plugin never crawls session storage. Local-only, no network construction. |
| D3 | `95d834cc` | Resumable checkpoints + dedup. Host-persisted atomic bounded checkpoints; source-range digests prevent duplicate ingest across resume; cancellation-safe; metadata-only `memory_import.progress` events. |
| D4 | `8eb2beea` | Import test battery + 1M scale. New `tests/memory_history_import.rs` covering every spec §20.4 bullet + a bounded 1M-turn ignored benchmark. |

## Gate evidence (spec §22 Phase D)

Historical sessions become searchable **without plugin filesystem crawling or
cross-project leakage**:

- `old_json_and_journal_backed_sessions_import_through_the_same_api`
- `cross_project_sessions_are_excluded_before_body_open`
- `prompt_system_and_secret_exclusion_sentinels_are_absent_from_imported_records`
- `import_resumes_after_forced_kill_without_duplicate_records`
- `declined_consent_reads_only_metadata_and_writes_nothing_to_axel`

## Verification (capped)

- `cargo test -p synaps-engine memory` → 105 passed, 0 failed
- `cargo test -p synaps-engine --test memory_history_import -- --test-threads=1` → 7 passed, 1 ignored, 0 failed
- ignored 1M benchmark (release): `batches=31250 throughput≈3.8e8 turns/s
  peak_bounded_state=32 records/1280 bytes rss_delta=4096 bytes` — bounded state
  and flat memory confirm unbounded corpora import within fixed bounds.

## Orchestration notes

- D1 required two worker runs: the first exhausted context leaving a
  non-compiling partial; the second finished it. Both times the foreman
  independently verified before committing.
- D3 and D4 workers each ran a workspace-wide `cargo fmt` that churned ~75
  unrelated files. The foreman steered mid-run to revert the cosmetic churn and
  commit only the feature/test files. Both final commits are focused (2 files
  each). No adversarial assertion was weakened.

## Disposition

CP-D passes on informed review. Proceed to Phase E (advanced retrieval and
consolidation).
