# Explicit memory migration and retention

Operator commands only; examples below have **not** been run against user data.
Migration never changes configuration, deletes its source, or enables dual writes.
Stop all source/destination writers before preview/apply/export. Keep private
backups. Do not delete SQLite WAL/journal files to bypass checkpoint refusal.

See [shared Axel repositories](shared-axel-repositories.md) for one shared brain,
Git worktree identity, defaults, and the host/service `/2` compatibility boundary.
Run commands from the intended repository or one of its verified worktrees.
With global `memory.backend = axel`, no per-project profile is needed. If using
an existing explicit profile, add `--profile NAME` to every command consistently.

## 1. Inspect repository identity

```sh
synaps retention memory-scope
```

This reports the stable canonical repository key, current legacy path key,
verified original member scopes/roots, and a mapping digest. It does not read
memory; first discovery may create the private Git-common-dir identity marker.
A random repository key prevents an unrelated repo at a reused pathname from
inheriting data. Existing path keys are migration candidates, not implicit grants.
Never convert a plugin `proj_…` alias to a host `p…` key by changing its prefix.

## 2. Built-in notes and eligible context archives, across worktrees

```sh
synaps retention migrate-memory --dry-run
synaps retention migrate-memory --apply \
  --project CANONICAL_KEY_FROM_PREVIEW \
  --manifest-digest EXACT_PREVIEW_SHA256
```

Preview includes every host-verified original main/worktree scope, including
historical paths retained in the private marker. It prints bounded counts/digests,
not bodies. Apply rereads and verifies the complete preview, then imports each
scope under its **original** key and explicitly links it to the repository group.
This keeps IDs, archive/capture digests and deletion evidence intact.

Every source import and every alias link is its own bounded atomic operation.
The batch is **not globally atomic**: a later failure leaves earlier acknowledged
imports/links intact. The report names completed and failed operations and any
unconfirmed outcome. Reconcile or retry the exact preview/options; never invent
replacement IDs. Source files and configuration remain unchanged.

## 3. Old plugin or native host brains / private exports

Supply an explicitly selected absolute source path and authoritative source key.
The target must be an exact host-verified original scope from `memory-scope`.
For native host exports, source and target scope must be identical; do not rewrite
capture/history identities to make an inventory fit.

```sh
synaps retention migrate-memory \
  --source-brain /absolute/old-brain.r8 \
  --source-project EXACT_SOURCE_KEY \
  --target-project VERIFIED_ORIGINAL_HOST_KEY \
  --project CANONICAL_REPOSITORY_KEY

# Review the preview, then repeat IDENTICAL source arguments plus:
# --apply --manifest-digest EXACT_PREVIEW_SHA256
```

`legacy_export` recognizes native host inventories and preserves captures,
archives, and fingerprints rather than flattening them into plugin notes.
Sources must be private, symlink-free, quiescent and checkpointed; they are opened
read-only and never initialized/migrated in place. Ordinary plugin source aliases
are explicitly mapped; native source payload identities are not changed.

To import a full private inventory, substitute:

```sh
synaps retention migrate-memory \
  --source-export /absolute/private-export/axel-memory.json \
  --source-project EXACT_ORIGINAL_HOST_KEY \
  --target-project EXACT_ORIGINAL_HOST_KEY \
  --project CANONICAL_REPOSITORY_KEY
```

External `synaps-axel-export/1` inventories require exact `format`,
`source_project`, and `target_project`, including empty/tombstone-only inventories.
Files must be private (0600 or stricter) and owner-controlled. Unsupported or
conflicting policies fail closed. Secret/restricted bodies stay in local operator
memory; preview/output never prints them. Do not paste full exports into model tools.

## 4. Explicit linking of already-present original scopes

```sh
synaps retention memory-scope --link VERIFIED_ORIGINAL_KEY
synaps retention memory-scope --link VERIFIED_ORIGINAL_KEY \
  --apply --project CANONICAL_KEY --manifest-digest EXACT_LINK_PREVIEW_SHA256
```

Repeat `--link` for additional verified original scopes. Linking shares the group’s
existing data and suppression evidence without rewriting records. The service
validates the private repository marker and Git-common-dir membership. A reused
historical path occupied by another repository is refused. No remote-URL matching
or arbitrary group reassignment is allowed. Explicit user notes cannot be linked.

## 5. Optional upgrade of an existing pinned host brain

A `/1/project` pinned brain remains pinned with the `/2` service. To reuse that
file as the shared destination (instead of migrating into a new shared file):

```sh
synaps retention upgrade-memory --project EXACT_ORIGINAL_PIN
synaps retention upgrade-memory --project EXACT_ORIGINAL_PIN \
  --apply --manifest-digest EXACT_UPGRADE_PREVIEW_SHA256
```

The configured destination is the file being upgraded. Preview hashes the
quiescent private file and verified mapping; the service validates/converts the
schema marker. This is separate from importing/linking data or changing config.
Do not run old writers against the upgraded file. After an ambiguous result,
inspect service membership rather than assuming the old digest still applies.

## Verify, back up, and retain

```sh
synaps retention memory-scope --service
synaps retention inspect
synaps retention export /absolute/NEW-private-export-directory
```

Inspection covers the selected repository membership group. Shared database
allocation is reported separately from scoped logical payload bytes. A repository
backup must retain all member inventories and its `axel-memory-index.json`,
including empty or bodyless deletion evidence. Single-owner exports retain
`axel-memory.json`; multiple owners use `axel-memory-<original-key>.json`. The
index is published last and lists owners, filenames, sizes and SHA-256 hashes. Service exports are exact original-owner inventories;
never flatten histories/captures from several owners into one invented scope.
Exports across members require quiescent writers for consistency and are not one
global transaction. Restore each inventory under its original verified scope,
then explicitly restore the verified alias links.

Existing limits: 24 MiB per atomic migration, 16,384 note/tombstone entries,
128 histories; larger inputs fail whole without truncation. Sweeps preserve
history/tombstones and never delete another project's data to meet a shared-file
size target. Logical deletion is not secure erasure of backups/media.

Saved notes/eligible archives are not every conversation transcript. Prior-session
import remains separately consented via `/memory index-history` and confirmation
with a live capture lease. Migration does not silently enable it.
