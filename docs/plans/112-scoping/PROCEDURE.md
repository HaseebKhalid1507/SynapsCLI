# Playbook: merge-large-branch — re-landing a big divergent branch as reviewable commits

**When:** a branch with tens of thousands of lines and 15+ conflict files must land on a base that moved underneath it (e.g. #112 onto daemon-mode dev). **Origin:** S326 scoping of #112; S235 scar (never `--theirs`); #114's failure mode (one blob merge, unreviewable).

## Principle
**Rebuild, don't merge.** Re-land the branch as small phase commits on the base, each buildable + tested, then record a bookkeeping merge commit so history shows the branch merged. Resolution lives in reviewable commits; the merge commit is a no-op.

## Setup
```
git worktree add wt/ref-merge <base>        # git merge --no-commit <branch>; leave markers; NEVER commit
git worktree add wt/ref-prior <prior-attempt> # optional: an earlier resolution for second opinion
git worktree add -b integration/<name> wt/build <base>
```
Pre-commit guard in `wt/build`: refuse commits containing `<<<<<<<|>>>>>>>|=======` lines. Hard rule: no `git checkout --theirs/--ours`, no `-X theirs/ours`.

## Phase plan
Input: a scoping doc that groups files into ordered phases with per-file semantic notes ("keep both: X + Y", "take branch's Z wholesale, base helpers are local-only"). Phases are ordered by dependency (types → core → engine → integration) and risk-first inside a layer.

## Per-phase loop
1. Pure additions: `git checkout <branch> -- <new paths>` (authorship preserved, no judgment).
2. Conflict files: read hunks in `wt/ref-merge`; apply by hand in `wt/build` per the semantic notes.
3. **Two-sided diff check** (`scripts/merge/twosided.sh <base> <branch> <files>`):
   - `git diff <branch>..HEAD -- f` must contain only base-side changes
   - `git diff <base>..HEAD -- f` must contain only branch-side changes
   Anything else = lost hunk. These two diffs are what reviewers read.
4. Build gate (bella only): `cargo check -p <crate>` → `cargo test -p <crate>` → commit. Message: phase name, files, `Co-authored-by:` original author, source commit shas.
5. Test ledger: append `phase | crate | passed | failed | ignored` to `LEDGER.md`. Any decrease = stop the line.

## Checkpoints
- After the riskiest phase: adversarial review (shady) of that file's two-sided diffs + Jawz read.
- After integration phase: whole workspace on bella; branch's integration tests run unchanged.
- Final: live verification list from the scoping doc; ledger total ≈ base + branch's new tests, none missing.

## Landing
`git merge --no-commit <branch>` in `wt/build` → take HEAD's tree wholesale (`git checkout HEAD -- . && git clean -fd`) → commit "merge <branch> (tree = phases N..M)". PR body: per-file table with two-sided diff line counts, the ledger, live-verification log. Stacked on whatever it depends on.

## Roles
One implementer per phase, sequential (same files). Orchestrator reviews two-sided diffs + ledger at every phase boundary and gates checkpoints. Brief = the phase text verbatim + this loop.

## Anti-patterns
- One giant merge commit (unreviewable, unbisectable).
- Parallel implementers on overlapping files.
- Fixing unrelated bugs "while in there" — except where the scoping doc explicitly co-locates them (same region, ≤10 lines, with its own test).
- Trusting a prior resolution for files the branch touched after it was made.

## Lessons from the first run (#112 → PR #120, S327)
- **Take git's clean auto-merges as a phase of their own.** The conflict files are the loud ~20%; the ~90 files git merged cleanly are silent, and hand-phases only touch what the scoping doc names. We lost ~3.6k lines (57 files) until a tests-only pass found ~86 upstream unit tests with no home. Add a phase early: `for f in <auto-merged>: cp ref-merge/$f build/$f; re-apply our-side hunks` — then the conflict phases graft onto a complete base.
- **Dropped-test count is the lost-hunk detector.** Per file: `grep -c '^\s*fn [a-z_0-9]*()' <upstream> vs <ours> vs <base>`; `ours == base < upstream` means upstream's `#[cfg(test)]` block never landed — and usually the production code it tests didn't either. Run this after every phase, not at the end.
- **Cut ref-merge on the FINAL base**, or rebase the build branch before the biggest conflict file — we cut on identity-w1, then rebased onto identity-w2 mid-run and re-resolved stream.rs twice.
- **Workers will "quick `cargo check`" on the wrong box.** Three did, past a bold rule. A shim (`~/.local/bin/cargo` → exit 86) fixed what the brief couldn't.
- **Worker budgets**: 64 rounds × 3 renewals dies at ~19 min on a 30-file hand-merge. Raise `turn_budget.worker.*` before dispatching (#391), and read the completion event's failure reason — `budget_exceeded` was buried in a preview string.
- **Adversarial review after the riskiest phase paid twice**: phase 3 (lost `drain_steering`, memory capture reading the pre-inference prompt), checkpoint B (§4 cwd-before-binding ordering, `launch_cancel` parity). Budget it.
- Landing: `git checkout HEAD -- .` after the `--no-commit` merge does NOT drop untracked additions from the branch — `git diff --cached --name-only` and decide each (we kept upstream's docs, dropped the TUI driver files).
