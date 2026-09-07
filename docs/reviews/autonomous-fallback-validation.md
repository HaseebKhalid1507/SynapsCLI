# Autonomous fallback follow-up — 2026-09-06

## Diagnosis and changes

The original loop treated every successfully completed foreground turn as progress,
reset retries and retained the same favorite. No progress/repetition information
reached the external plugin. Several exact host-generated provider failure messages
also fell through to blocked instead of delivering a known provider-error class.

Implemented and installed autonomous plugin **0.1.1** plus its updated host:

- Explicit optional `feedback_version:1` on Start, pinned to the grant. Legacy
  plugins keep their six-field poll payload; opted-in polls add only a validated
  `unknown|changed|repeated|empty` label.
- Bounded process-local tracker: ring of four successful turn fingerprints,
  reset on exact model/effort change; 1 MiB byte, 1,024 tool-event, 16,384 JSON-node
  and depth-64 limits. Overflow is unknown, never a prefix comparison. Tool IDs,
  thinking and tool-associated prose do not influence tool signatures. No content
  or fingerprints are exported or persisted by the tracker.
- Three consecutive repeated/empty successful-turn signals advance the exact
  favorite. Changed/unknown/missing feedback and failed attempts reset the streak.
  Successful-turn and duration limits take precedence. Duplicate decisions cannot
  double-count. Recovery prompts and notices identify fallback and require a
  different approach without replaying side effects or bypassing gates.
- Exact Anthropic SSE/network templates and Responses empty/missing-terminal
  templates now retain retry eligibility. Responses auth/quota wire enum classes
  get dedicated static messages rather than collapsing into generic failure.
- xAI's validated broker HTTP-status envelope is normalized into the existing
  API-error path, retaining vetted quota labels instead of becoming config_error.
- Review fixes: ambiguous auth_error (broker machine auth, policy, storage or
  transport) stays blocked; only exact host missing-account templates qualify.
  Explicit unknown/policy/conflicting Responses identifiers outrank free-text
  capacity hints. Unknown, context, incomplete, tool, policy, budget and
  interrupted-side-effect failures are not promoted into retries by feedback.

## Verification

All commands were locked/offline with at most eight build workers and one test
thread; no parallel Cargo jobs were launched by this task.

- Focused checks: 17 driver-engine tests, two error-path tests, 41 TUI driver and
  tracker tests, five real external-plugin integrations passed.
- Python plugin suite: **47 passed**, both source and installed copies.
- Full workspace was run after production fixes: **4,151 passed, four failed,
  34 ignored**. All four failures were older expected strings/categories for
  explicit Responses server_error, now correctly transient instead of generic.
  Updated those four expectations, then reran the entire failed engine-lib
  target: **1,967 passed, zero failed, 11 ignored**. No production code changed
  after the full workspace run. Across that run plus the complete target rerun,
  all **4,155 non-ignored tests** pass; this is not a claim of a third clean
  single-command workspace run.
- Production strict Clippy (`--workspace --lib --bins -- -D warnings`) passed.
- Release build passed (3m37s); `--version`, `--help`, artifact/PATH hash equality,
  installed passive initialize/status/shutdown, diff whitespace and LOC ratchet
  checks passed. Passive smoke never invoked start.

Logs and receipt: `/tmp/synaps-auto-fallback.rW7RcDyz/`. Relevant files:
`workspace-final.log`, `engine-final.log`, `clippy-final.log`, `release.log`,
`installed-python.log`, `install-receipt.json`, and accompanying `.exit` files.
An earlier full pass inherited PTY stdin and waited for the offline cloud-login
failure test's AWS URL prompt; EOF completed it. The final pass used `/dev/null`.

## Installed state

- PATH host: `/home/jr/.cargo/bin/synaps`
- SHA-256: `dd85a6afd947cf990ca7b0f792b453df82547bab12e30611cde70168abc467d3`
- Plugin: `/home/jr/.synaps-cli/plugins/autonomous`, version 0.1.1
- Private previous host/plugin backup:
  `/home/jr/.synaps-cli/.autonomous-update-backup-82ild3xs/`

Host publication used a staged copy and atomic replacement. Plugin publication
used atomic directory exchange. Config, existing plugin-local preferences and
other installed plugin data were preserved. No commit, reset, running-session
restart, live autonomous run or paid-provider smoke test was performed. Existing
uncommitted repository edits were preserved.

## Limits and deferred findings

This is exact-repetition detection **between completed foreground turns**, not a
semantic progress evaluator or interruption of one still-running tool loop.
Changing output/order can evade detection; repeated legitimate observations can
trigger it. Hashes are noncryptographic process-local heuristics. Unknown/overflow
never triggers recovery. User cancellation and normal safety/permission gates
remain authoritative. Existing sessions need a fresh TUI to use the update.

Favorites remain exact and unchanged. Unsupported model/provider spellings are
visibly skipped, not rewritten. No claim that all requested favorites are usable.

Read-only audit also identified older provider-decoder acceptance gaps (OpenAI
Chat error envelopes/clean EOF, Anthropic partial clean EOF, permissive Responses
success markers). They are separate from this bounded fallback change and were
not changed or claimed fixed. Unknown transport/provider failures still fail
closed where no safe class survives.
