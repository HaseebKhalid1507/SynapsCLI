# Archive eligible-input budget fix

## Report and cause

`/context auto` could fail with `archive commit failed; history retained: ... archive input byte budget exceeded` even when the eligible archive was small.

The old `core/context_archive.rs::project_hashed` walked the **entire raw JSON history** before applying archive exclusions. System/developer messages, private reasoning, attachments, ignored metadata and derived continuation notes collectively consumed the 16 MiB input limit despite never being archived. The reported correlation's private session/trace was not needed or inspected; synthetic inputs reproduce the defect.

## Implemented contract

- Complete a semantic preflight for **all messages before any payload screening, cloning or serialization**. Share message-envelope selection with the projector and whole-tool-input admission between preflight and projection. Identifier screening remains bounded to 128-byte identifiers.
- Preserve the **16 MiB eligible-source byte limit**, 4,096 messages, 65,536 visited nodes, depth 32, 8 KiB notes, and existing final segment/store/fetch bounds. Count admitted string/key bytes with the existing per-node structural allowance; do not introduce extra charges beyond the corresponding raw subtree.
- Skip excluded message/block payloads and ignored metadata without descending into them. Visited block roots still consume structural budget; descendants of an omitted blob do not. Malformed content arrays do not recursively become content blocks.
- Skip already-withheld tool arguments/results when no trusted redactor is installed, checkpoint tool arguments, and after-redaction text for which no redactor exists. Existing placeholders remain unchanged.
- Tool-input eligibility is **whole-argument and transactional**: measure tentative bytes with saturation, enforce depth/node bounds, and charge bytes only if all visited children are eligible. A later forbidden disclosure withholds the entire input even when an earlier field is oversized. Structural visits are never rolled back.
- Arbitrary nested tool JSON is not interpreted as a content-block array. Unclassified `data`/`base64` fields remain eligible, count toward input limits, and retain their previous behavior. Explicit disclosure exclusion still withholds the whole argument. No new binary-shape heuristic or policy is introduced.
- Serialize admitted tool arguments with a bounded writer. Scratch limit is six times measured argument cost, covering JSON escaping and punctuation/numbers. An admitted argument may escape above 16 MiB and subsequently shrink through a trusted redactor, as it could before this fix. Final output limits remain unchanged. The redactor itself is trusted; its callback interface cannot prevent arbitrary internal allocation.
- Preserve exact accepted text/placeholders, source indices, block indices, original source count, logical-ID hashing and digest algorithm. No prefiltering/renumbering, truncation, retention change, dropped history or fallback backend.
- Both legacy and Axel `history_seal` use this projection. Eligible overflow occurs before an Axel seal RPC or local segment publication. The runtime retains its source/window/note on failure; successful seal still does **not** commit an active head without the existing durable-head acknowledgement.

## Regressions

Core tests cover aggregate excluded blobs above 16 MiB, projection/digest equivalence to tiny excluded blobs, eligible aggregate failure before the first redactor call, exact multi-MiB tool logs, already-withheld payloads, both traversal orders for whole-argument withholding, nested binary-looking input compatibility, structural caps, and escape-expanding scratch input. Golden digests were captured against the pre-change projector and are asserted for redacted and unredacted fixtures.

Engine tests exercise legacy and real Axel history round trips, retry deduplication, tombstone replay, atomic overflow refusal and no-backend-call failure; a rollover regression covers the unchanged head barrier and failed source/window/note preservation. All use synthetic data. Validation results are recorded in `docs/reviews/forum-archive-validation.md`.

## Deliberate limits

This does not solve a genuinely oversized eligible tool log or a successor that cannot safely reduce context. Those still stop without silently losing evidence. It does not alter permissions, consent, provider budgets, worker checkpoint support or the separate forum protocol. No private data migration, session restart or installed-binary replacement accompanies this source change.
