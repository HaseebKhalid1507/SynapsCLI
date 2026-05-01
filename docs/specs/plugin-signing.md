# Synaps Plugin Checksums and Signing

**Status:** Draft design  
**Scope:** Distribution trust model for plugin indexes and future package artifacts.

## Goals

- Ensure downloaded plugin content matches index metadata before install.
- Make update verification deterministic and local-first.
- Leave room for optional publisher signatures without requiring a central authority.

## Non-goals

- No mandatory public-key infrastructure in v1.
- No remote attestation.
- No guarantee that verified code is safe; permissions/trust confirmation remains required.

## Checksums

Plugin index entries should include:

```json
{
  "checksum": {
    "algorithm": "sha256",
    "value": "hex-digest"
  }
}
```

Supported algorithm for v1: `sha256`.

The digest target depends on distribution form:

- **Package archive:** digest the exact archive bytes.
- **Git source:** digest is advisory unless paired with a pinned commit; installers should record repository URL + commit.
- **Local directory:** digest can be omitted or computed by tooling for dry-run inspection only.

## Optional signatures

A future index entry may include:

```json
{
  "signature": {
    "algorithm": "minisign-ed25519",
    "key_id": "publisher-key-id",
    "value": "base64-signature"
  }
}
```

Signature verification should cover the checksum-bearing package metadata, not mutable remote state.

## Install verification

1. Fetch package or source to pending install.
2. Verify checksum when present and applicable.
3. Verify signature when present and trusted locally.
4. Re-read `.synaps-plugin/plugin.json` from fetched content.
5. Show permissions, hooks, commands, config keys, publisher, and source.
6. Move into final plugin directory only after user confirmation.

## Update verification

- Fetch updates into a pending-update directory.
- Verify checksum/signature before replacing installed content.
- Compare old vs new permissions, hooks, commands, extension command path, and config keys.
- Require confirmation when executable capabilities change.
- Failed verification leaves the installed plugin untouched.

## Local trust store

Synaps may maintain a local trust store under `$SYNAPS_BASE_DIR` containing approved publisher keys and previously accepted plugin sources. Trust decisions are local user choices, not global endorsements.

## Security notes

- Checksums protect integrity, not intent.
- Signatures identify a signing key, not universal safety.
- Index metadata is untrusted input; installers must validate manifest content after fetch.
- Secrets must never be requested or resolved during package dry-runs, indexing, or checksum verification.
