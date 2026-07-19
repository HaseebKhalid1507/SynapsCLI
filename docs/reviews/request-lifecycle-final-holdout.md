# Request Lifecycle Hardening — Final Program Holdout

- **Verdict:** PASS
- **Score:** 0.95 / 1.00 (weighted exact value: 0.945)
- **Spec fidelity:** 0.94 / 1.00
- **Critical findings:** 0
- **Important findings:** 0
- **Reviewed range:** `d20e03f..65e6e3c6`
- **Reviewed HEAD:** `65e6e3c63cd4308bfa211246a8240c1c92e7cbf4`
- **Reviewer:** fresh independent holdout Judge (`openai-codex/gpt-5.6-sol`), read-only

## Scorecard

| Axis | Weight | Score | Weighted |
|---|---:|---:|---:|
| Security/privacy | 0.35 | 0.96 | 0.336 |
| Correctness | 0.30 | 0.94 | 0.282 |
| Spec fidelity | 0.20 | 0.94 | 0.188 |
| Code quality | 0.10 | 0.91 | 0.091 |
| Documentation | 0.05 | 0.96 | 0.048 |
| **Total** | **1.00** |  | **0.945** |

## Gate disposition

All five phases and Tasks 1–36 are implemented. The final blocker—T35
session persistence following symlinked ancestors above the sessions
directory—was closed in `65e6e3c6`. On Unix, complete absolute session paths
are resolved from an opened `/` handle with `openat2(RESOLVE_BENEATH |
RESOLVE_NO_SYMLINKS)` or a component-by-component `openat(O_NOFOLLOW |
O_DIRECTORY)` fallback. All T35 reads, writes, appends, deletes, listings, and
metadata reads are then handle-relative. Relative paths and `.`/`..` fail
closed. Legitimately symlinked homes require a canonical `SYNAPS_BASE_DIR`, as
documented.

The Judge found no remaining Critical, Important, Moderate, or Minor issue
requiring remediation for this gate. Deliberate compatibility constraints are
documented: strong complete-path confinement is Unix-specific, and relative
or symlink-containing configured paths fail closed.

## Final evidence

Exact resource-capped parallel workspace gate:

```text
CARGO_BUILD_JOBS=8 cargo test --workspace --jobs 8 -- --test-threads=8
/tmp/cp14-fix2-workspace-test.log
114 summaries; 3361 passed; 0 failed; 18 ignored
```

Additional evidence:

- `cargo check --workspace --all-targets --jobs 8` — exit 0
- `cargo build --release --jobs 8` — exit 0
- `git diff --check` — clean
- targeted per-file `rustfmt --check` — clean
- T35 journal tests — 35/35
- all 23 `synaps-core` test binaries green
- phase harnesses 1–5 are included in the exact workspace run
- full evidence: `/tmp/cp14-fix2-evidence.md`

The final Judge personally inspected the strict path resolver, concurrent
ancestor-swap semantics, atomic/private handle-relative file operations,
listing and metadata paths, bounded reads, adversarial victim-sentinel tests,
prior-fix preservation, and retained gate totals. It did not modify files or
rerun Cargo commands.

The unrelated unstaged `docs/request-lifecycle-hardening-spec.md` edit was
excluded from the committed review and remains uncommitted by design.
