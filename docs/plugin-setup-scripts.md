# Plugin extension setup and prebuilt contract

Synaps runs post-install work after a plugin source tree is cloned or updated.

## Manifest fields

```json
{
  "extension": {
    "runtime": "process",
    "command": "extensions/my-ext/target/release/my-ext",
    "setup": "scripts/setup.sh",
    "prebuilt": {
      "linux-x86_64": {
        "url": "https://example.com/my-ext-linux-x86_64.tar.gz",
        "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
      }
    }
  }
}
```

- `extension.setup` takes precedence over legacy `provides.sidecar.setup`.
- `extension.command` for shipped binaries must be plugin-relative. Absolute paths and `..` traversal are rejected. Bare PATH commands remain supported for interpreter-style extensions.
- `extension.prebuilt` keys use compact host triples such as `linux-x86_64`, `linux-arm64`, `darwin-x86_64`, `darwin-arm64`, `windows-x86_64`, and `windows-arm64`.
- Prebuilt `url` must be HTTPS in production builds. `file://` is accepted only in tests.
- `sha256` must be exactly 64 hex characters; values are normalized to lowercase before comparison.

## Install order

1. If `extension.command` already resolves to an executable plugin-relative file, setup is skipped.
2. If a matching `extension.prebuilt` asset exists, Synaps downloads it with request timeouts and a 128 MiB size cap, verifies SHA-256 while streaming, extracts it through a sandbox, and verifies `extension.command`.
3. If no prebuilt matches, Synaps runs `extension.setup` (or legacy sidecar setup) with `bash` from the plugin directory.
4. After setup, Synaps verifies the declared extension binary exists and is executable.

## Prebuilt archive layout

Archives should contain files at the paths expected by the manifest, for example `extensions/my-ext/target/release/my-ext`. Flat plugin-relative layout is supported. Do not rely on a top-level wrapper directory unless your `extension.command` includes that directory.

Supported archive types: `.tar.gz`, `.tgz`, and `.zip`. Archive entries with absolute paths, `..`, special file types, or symlinks escaping the extraction root are rejected.

## Setup environment and platform policy

Setup scripts run as `bash <script>` with current directory set to the plugin root. The environment is reduced to a small allowlist (`PATH`, `HOME`, `USER`, `SHELL`) plus `SYNAPS_PLUGIN_DIR`; API keys and other parent-process secrets are not inherited by default. Windows setup scripts are not supported in this release; use prebuilt assets on Windows.

## Failure behavior

Plugins remain recorded after recoverable setup failures, but their setup status is persisted. Extensions with failed setup are skipped on later session start until setup succeeds via reinstall/update. Hard security failures such as checksum mismatch, unsafe URL, path escape, or invalid command are surfaced clearly and should not be ignored.

Troubleshooting:

- Missing binary: check the setup log path shown in `/plugins`, run the setup script from the plugin directory, then reinstall/update.
- Checksum mismatch: publish a new manifest SHA-256 or replace the corrupt asset.
- Archive rejected: remove absolute/traversal entries and avoid escaping symlinks.
