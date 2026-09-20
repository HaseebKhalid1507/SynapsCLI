# Autonomous redundant-confirmation recovery validation

## Scope

External reference plugin 0.1.5 fixes a prompt-policy gap where the model may
require a magic confirmation phrase even for clearly authorized remaining work.
No Rust production behavior, host grants, permission enforcement, feedback wire
fields, model favorites, retry policy or cancellation behavior changed.

Changed implementation: `examples/extensions/autonomous/main.py` and manifest.
Regression coverage: plugin Python suite and `tests/autonomous_plugin.rs`.
README and implementation contract document the behavior and limitations.

- Every initial, continuation and recovery prompt identifies automated origin,
  not new human input/approval. Latest actual human steering controls; the stored
  original goal remains historical context.
- Clearly authorized next steps do not require assistant-invented reconfirmation.
  Never manufacture approval, override explicit human review checkpoints, expand
  scope, replay completed effects, or retry a genuinely blocked action.
- First/second consecutive successful `repeated`/`empty` feedback labels send a
  fixed re-evaluation prompt on the same favorite after one second. This does
  not reset the streak; third-label failover and full-wrap cooldown are unchanged.
- Feedback is a heuristic, not proof of consent or a semantic approval classifier.
  No transcript prose or additional feedback metadata is exported.
- Turn/deadline limits precede correction; duplicate callbacks remain idempotent;
  typed blocked outcomes remain terminal. Changed/unknown/missing feedback
  restores the ordinary continuation prompt.

## Offline verification

All Rust build invocations used `CARGO_BUILD_JOBS=8` / `-j 8`. Rust tests ran
serially (`RUST_TEST_THREADS=1`, `--test-threads=1`). No parallel Cargo jobs were
started during the workspace run.

| Check | Result |
| --- | --- |
| `python3 -m unittest discover -s examples/extensions/autonomous/tests -v` | 61 passed, including real subprocess framing fixtures |
| `cargo test --offline -j 8 --test autonomous_plugin -- --test-threads=1` | 7 passed |
| `cargo test --workspace --offline -j 8 -- --test-threads=1` | 4,258 passed; 38 ignored; 124 successful suite summaries, including final doctests |
| `cargo clippy --offline -j 8 --test autonomous_plugin -- -D warnings` | Passed |
| `rustfmt --edition 2021 --check tests/autonomous_plugin.rs` | Passed |
| Python `ast.parse(..., feature_version=(3,8))` on plugin and tests | Passed (syntax compatibility check, not execution under Python 3.8) |
| Scoped UTF-8/replacement-character check and `git diff --check` | Passed |

Local evidence logs: `/tmp/synaps-auto-confirmation-{python,integration,workspace,clippy,focused-clippy}.log`.
The workspace command exceeded the tool wrapper's 240-second wait, but its Cargo
process continued. It was monitored until exit; the log contains all 124
successful suite summaries through the final TUI doctests, without failures.
The original wrapper did not return a final process exit-code receipt.

Workspace-wide strict Clippy (`--workspace --all-targets -- -D warnings`) is
**not green**: it stops on existing warnings in files unchanged by this task:

- `crates/agent-core/src/core/auth/github_copilot.rs:967,969`: `type_complexity`.
- Same file, line 1111: `assertions_on_constants`.
- `crates/agent-core/tests/provider_confinement.rs:65`: `cloned_ref_to_slice_refs`.

No unrelated lint fixes or suppressions were added.

A read-only reviewer checked the design and final source. No blocking regression
was reported. Follow-up cleaned stale version/feedback documentation and added
an explicit first-empty-feedback wording assertion.

## Limits and deployment

Tests establish policy, framing and accounting behavior, not provider compliance.
Different wording can evade exact-repetition detection. A prose-only approval
request or completion still counts as success, not a structured driver stop;
automated turns and charges may continue until cancellation or a configured
limit. Genuine gates remain enforced independently; a model switch supplies no
approval.

No live inference validation, plugin installation/reload, release build, PATH
binary replacement, preference/configuration edit or migration was performed.
This patch needs only the updated external plugin on the existing compatible
host. Existing unrelated worktree changes were preserved.
