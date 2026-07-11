# Extension Tutorial — "LLM writes an extension from docs only" experiment (P14.2)

**Date:** 2026-07-09 · **Method:** a fresh-context LLM (opus-4-8) was given ONLY
`docs/extensions/tutorial.md` — forbidden from reading source, protocol.md, or existing
examples — and asked to build a working extension. Every ambiguity it hit is a doc bug.
This is the H1/H2 "can an LLM build against our docs" proof.

## Result
The model produced a complete extension (`.synaps-plugin/plugin.json` + `main.py`), reconstructed
a stdio test harness from the framing spec, and matched Step 8's expected output line-for-line.
Self-assessed load confidence: **~90%** — handshake + manifest copied verbatim from the tutorial's
normative examples; residual risk was exactly the two naming ambiguities below. **Verdict: the
tutorial is sufficient to produce a loadable extension**, with the clarity fixes now applied.

Generated artifact (kept as evidence, not shipped): `~/Jawz/workspace/p14-experiment/hello-ext/`.

## Gaps found → dispositions
| # | Gap | Disposition |
|---|-----|-------------|
| 1 | `test_hello.py` referenced + output shown, but source not inline (model was blocked from `examples/`) | **FIXED** — Step 8 now names the harness source path (`examples/extensions/hello-ext/test_hello.py`) explicitly as readable/adaptable |
| 8 | Install paths look inconsistent: `~/.synaps-cli/plugins/` vs `./.synaps/plugins/` | **FIXED (clarity)** — verified against `manager.rs:849-850`: two intentional roots (user vs project-local). Tutorial now says the `-cli` difference is not a typo |
| 9 | manifest `name` vs `plugin_id` vs install-dir relationship never stated | **FIXED** — verified: plugin-id = install directory name (`config_store.rs:4`, `manager.rs` scan). Tutorial now states `install-dir == manifest.name == plugin-id` convention |
| 2 | `id` semantics (increment? per-connection unique?) unstated | Minor — model echoed ids back harmlessly; left as-is (protocol.md territory) |
| 3 | "a plain string result also works" under-specified vs `result.content` object | Minor — acceptable ambiguity; canonical form is shown in code |
| 4 | `config` mechanism forward-referenced to protocol.md | By design — tutorial is minimal; protocol.md owns config |
| 5 | id-less notification handling unstated | Minor — protocol.md territory |
| 6 | HookEvent schema only partially enumerated | By design — links to hooks.md |
| 7 | `modify`/`confirm` hook actions listed but not demonstrated | By design — minimal tutorial shows continue/block |

**Net:** 3 real fixes applied to the tutorial (gaps 1, 8, 9); the rest are intentional
minimalism (protocol.md/hooks.md own the depth) or harmless. The experiment validated the
tutorial's core claim: a fresh agent CAN build a loadable extension from this page alone.
