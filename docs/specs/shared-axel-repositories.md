# Shared Axel brain and repository identity

## Behavior

Axel now supports **one user-owned database with separate logical scopes**, not
one database/profile per checkout. Main checkouts and their registered Git
worktrees share a durable repository key. Unrelated repositories remain isolated.
Moving a repository together with its Git common directory preserves that key;
creating an unrelated repository at the old path does not inherit it.

This is an implementation change, not a claim that existing user data/config was
migrated. Build/test operations use synthetic repositories and databases only.

## Configure once

Build the host and independently pinned service together. `/2` hosts/services
require the matching wire contract; `/1` services or hosts fail closed, without
opening an alternate store. The pinned upstream Axel revision remains unchanged.

```sh
CARGO_BUILD_JOBS=8 cargo build --release --locked --offline --bin synaps
CARGO_BUILD_JOBS=8 cargo build --manifest-path sidecars/axel-memory-service/Cargo.toml \
  --release --locked --offline
```

Install both executables in the same directory. In the desired normal config:

```text
memory.backend = axel
# Optional if installed next to synaps:
# memory.axel.executable = /absolute/path/to/synaps-axel-memory-service
# Optional shared destination override:
# memory.axel.brain = /absolute/private/directory/brain.r8
# Separate explicit opt-in; not required for repository/worktree sharing:
# memory.user_scope = true
```

With no brain override, the host provisions a private
`<SYNAPS_BASE_DIR>/memory/axel/brain.r8` (normally
`~/.synaps-cli/memory/axel/brain.r8`). The directory is 0700 and files 0600.
Explicit destinations are never silently adopted or permission-repaired.
Executable selection is an absolute sibling path, not a PATH/shell fallback.

Profiles remain supported but are not needed per project/worktree. Existing
profile-specific `memory.axel.brain` overrides still select those files; a shared
configuration does not override an explicit profile. Backend/path/user-scope
policy is captured at runtime startup; restart after changing it.

## Identity and worktrees

On first Axel repository discovery, local read-only Git plumbing resolves the
common Git directory and verifies registered worktree backlinks. It creates:

- `<git-common-dir>/synaps-memory-identity.json` (private durable identity)
- `<git-common-dir>/synaps-memory-identity.lock` (persistent advisory lock)

The version-2 marker holds a **random** `p<16hex>` repository key, its initial
root, and verified historical/current root paths. A key must not be seeded from
the pathname: that would let a new repository at a reused path see old data.
Roots are provenance and operator migration candidates, not automatic read grants.
The marker is private, single-link, owner-checked, atomically published and synced.
Malformed/unsafe state stops Axel identity resolution, rather than generating a
replacement or falling back to a path scope. Normal owner-owned 0775 repository
directories are supported without chmod; marker/lock files remain 0600.

Git queries and marker-lock acquisition are bounded. No network, hooks, remote
matching, or automatic linking of independent clones occurs. Copying Git-private
identity files copies identity: do not manually copy the marker to an unrelated
repository. Keep it when moving the same repository. Git worktree moves/repairs
must leave valid Git metadata. Bare/non-Git and explicit override behavior is
reported by `memory-scope`; non-Git directories retain legacy path scoping.
`SYNAPS_PROJECT_ROOT` is an explicit path-identity override and bypasses automatic
Git sharing, so remove accidental inherited overrides for normal worktree use.

```sh
cd /path/to/any/worktree
synaps retention memory-scope
synaps retention memory-scope --service
```

The first reports identity/mapping metadata, without opening memory. It may
initialize/update the Git-private marker. `--service` additionally opens the
selected brain and reports canonical/member scopes. A changed identity does not
silently search old JSONL or old plugin databases.

## One database, bounded access

Every operation still carries a host-selected scope. Reads, captures, archives,
receipts, tombstones, migration, and retention queries apply scope membership.
A shared SQLite file is not permission to query every project. Global record-ID
collisions fail closed rather than overwrite unrelated data. Private filesystem
permissions and the no-symlink boundary remain; this is not a same-UID sandbox.

Normal notes use the canonical repository scope. New host notes retain source
worktree identity metadata. Captures retain their session/turn provenance; context
archives retain their logical session IDs. Sharing does not turn branch-specific
evidence into universal truth or erase its provenance. Session active heads and
external tool effects are not made multi-process transactional by this change.

User-wide notes are **explicit**, not an automatic union in every query:

```text
memory_store(scope="user", content="…", sensitivity="normal")
memory_search(scope="user", query="…")
memory_fetch(scope="user", ids=["exact-returned-id"])
```

These require host `memory.user_scope = true`; default `scope="repository"`
retains normal repository access. The fixed user key is reserved and cannot join
repository aliases. User-scoped history/capture is refused. Automatic recall and
continuous capture remain repository-bound and require separate `/memory` consent.
Nothing here enables `/context auto`, prior-session import, or live model inference.

## Existing data continuity

See [migration commands](memory-migration.md). A new random repository key cannot
automatically adopt an old path hash without risking cross-repository disclosure.
Instead, metadata previews bind exact verified original scopes, source digests,
destination, and the requested alias sharing. Apply imports under **original**
keys, then explicitly links membership to the repository key. It preserves IDs,
capture payload/project digests, archive digests, disclosure, expiry, provenance,
and deletion evidence; no copying/resealing into newly invented IDs.

Links require current Git-common-dir/marker proof or retained historical roots;
a historical path reused by an unrelated live repository is refused. Membership
cannot be reassigned to another canonical group. Linking shares suppression
fingerprints, so forgetting wins over retries and imported copies of recorded
lineage. It is not semantic erasure of arbitrary paraphrases/backups.

Existing `/1/project` pinned host brains remain pinned when opened by `/2`.
`upgrade-memory` performs explicit digest-gated conversion to shared storage; it
does not import data, authorize aliases, or change configuration. Do not install
only one half of the host/service protocol pair or downgrade without a compatible
backup/export strategy.

## Retention, backups, and limits

Counts and `bytes` cover the selected membership group's logical payloads.
`database_bytes` is the shared SQLite allocation, informational only. Project
sweeping never deletes another project's records to meet a whole-file target.
History/tombstones remain protected unless explicitly forgotten; logical deletion
is not secure media erasure.

Service exports preserve **one original owner** at a time, because flattening
owners loses capture/history/deletion identity. The host repository export
includes every member as separate inventories and a final `axel-memory-index.json`; multi-scope backups
and migration batches are not global transactions. Stop writers for a consistent
backup/cutover, preserve all inventories (including bodyless deletion evidence),
and explicitly restore/link each original scope. Never paste private export
bodies into model tools.

Existing limits remain: Linux/procfs service, 15-second host call deadline,
1 MiB ordinary/24 MiB large frames, 24 MiB per atomic migration,
16,384 combined note/tombstone entries, and bounded history inventories. Oversized
operations fail without truncation. Repository discovery/mapping is bounded too.
This is not an unbounded “import all accounts/projects” optimizer.

## Verification record (shared-brain revision)

- Full workspace: **3,983 passed, 34 ignored, 1 filtered**, 121 result targets.
  Command: `cargo test -j 8 --workspace --offline --no-fail-fast -- --test-threads=1
  --skip ui_catalog_fetch_github_copilot_returns_prefixed_chat_models`.
  The filtered test is the previously documented ambient-account catalog issue.
- Separately executed against the real service: **9 existing/new runtime-memory
  tests**, **1 worktree/move/path-reuse/provenance roundtrip**, **1 user-note tool
  test**, and **2 real grouped-export/migration CLI tests**. Also rerun against
  the release service. Full shared-migration target: 10 passed (including those 2).
- Standalone service: **41 passed**; strict all-target Clippy `-D warnings` passed.
- Host all-target Clippy passed with pre-existing unrelated warnings. New shared
  modules/retention changes have no remaining reported Clippy warnings.
- Release builds for both executables succeeded. An actual release-host smoke
  copied the matching pair into a synthetic bin directory and verified
  configure-once defaults, sibling service resolution, shared main/worktree
  identity, unrelated-project isolation, private storage, original-key migration,
  unchanged source/config, and complete grouped-export hashes.
- Schema drift, TUI LOC/ignore ratchets, and `git diff --check` passed.
- Tests used temporary repositories/storage, not user-memory migration. Initial
  harness runs with `SYNAPS_BASE_DIR` forced globally broke HOME-based legacy test
  fixtures; final successful suite used isolated HOME without that override.
  A stale no-explicit-path test assumption was updated for the new default paths.

Evidence: `/tmp/shared-axel-workspace-final.log`,
`/tmp/shared-axel-service-final.log`, `/tmp/shared-release-*.log`,
`/tmp/shared-axel-clippy-final.log`, `/tmp/shared-axel-service-clippy-final.log`.
Temporary logs are not durable release attestations. Builds/tests are capped at
8 workers; sensitive test harnesses use a single test thread.
