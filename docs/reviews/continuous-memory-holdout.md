# Continuous Memory — Independent Security/Privacy Holdout Verdict

- **Plan:** `axel-continuous-memory-plan/1`, Task F5 (Phase F gate / CP-F)
- **Spec:** `axel-continuous-memory/1`
- **Reviewer:** independent read-only subagent, model `anthropic/claude-opus-4-8`
  (builder was `openai-codex/gpt-5.6-sol` — information wall preserved: reviewer
  saw only the spec, the diff, and the code/tests, never the builder's
  conversation).
- **Reviewed heads:** host `5ec0a648`, axel `b91dfcb`, plugin `fbdcb08`.
- **Rubric (fixed pre-run):** security/privacy 0.35, correctness 0.30, spec
  fidelity 0.20, code quality 0.10, docs 0.05; PASS = weighted ≥ 0.80 AND spec
  fidelity ≥ 0.70 AND zero Critical AND zero Important.

## Per-axis scores

| Axis | Score | Evidence |
|---|---:|---|
| Security/privacy | 0.92 | Lease constructor `pub(crate)`, `non_exhaustive`, no `Deserialize`; forged-enable denied and installs no lease; subagent starts off despite active parent lease; secret bodies never reach model context (host + Axel `recall_quality` secret_leakage=0); cross-project IDs fail closed. |
| Correctness | 0.88 | 105 memory unit tests + recall_quality gate green; durability/idempotency/no-inheritance oracles pass; C5 + integration_wire kill/reopen + oversized-frame oracles exist and referenced correctly. |
| Spec fidelity | 0.85 | Off-by-default, host-mediated enable, lower-authority typed segments, bounded queues, offline-first all mapped to real tests; §20.5 cross-references point to genuinely passing oracles (satisfies §23). |
| Code quality | 0.85 | Type-level guards (non_exhaustive, compile_fail doctests, fail-closed schema validation). |
| Docs | 0.85 | Thorough spec; self-documenting, traceable test names. |

## Findings

- **Critical:** none
- **Important:** none — the reviewer's open concern (that some §20.5 tests are
  "hollow cross-references") was resolved by confirming every referenced oracle
  genuinely exists and passes (`secret_bodies_never_reach_model_context`, Axel
  `recall_quality`, agent-core `private_fs.rs`, plugin `integration_wire.rs`),
  so they are documentation aliases, not hidden/serialized failures per §23.
- **Minor:** some §20.5 tests assert only `references.len()==2` / enum-matches-
  itself and rely on named oracles elsewhere; acceptable, but clearer with
  direct assertions or explicit `#[doc]` links to the backing oracle
  (`continuous_memory_adversarial.rs`). Accepted as-is for this release.

## Weighted total

```
0.92×0.35 + 0.88×0.30 + 0.85×0.20 + 0.85×0.10 + 0.85×0.05
= 0.3220 + 0.2640 + 0.1700 + 0.0850 + 0.0425
= 0.8835
```

- weighted 0.8835 ≥ 0.80 ✓
- spec fidelity 0.85 ≥ 0.70 ✓
- zero Critical ✓ · zero Important ✓

## GATE: PASS
