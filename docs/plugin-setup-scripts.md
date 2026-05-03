# Plugin Setup Scripts (Post-Install Build Hooks)

When a plugin ships native code (a Rust sidecar, a Python virtualenv,
a node_modules tree) the built artifact is typically gitignored — it's
platform-specific, large, and cheap to rebuild. The marketplace clones
source-only, which means a fresh install would otherwise leave the user
staring at:

```
⚠ Extension 'local-voice' failed: Failed to spawn extension
  'local-voice': No such file or directory (os error 2)
```

The setup-script hook closes that gap. Plugins that need a build step
declare a `provides.sidecar.setup` path in their manifest, and Synaps
auto-runs that script after the marketplace install completes.

## Manifest declaration

In `.synaps-plugin/plugin.json`:

```json
{
  "name": "local-voice",
  "extension": {
    "command": "bin/synaps-voice-plugin",
    "args": ["--extension-rpc"],
    ...
  },
  "provides": {
    "sidecar": {
      "command": "bin/synaps-voice-plugin",
      "setup": "scripts/setup.sh",
      "protocol_version": 1
    }
  }
}
```

The `setup` path is **relative to the plugin's root directory** (the
directory that contains `.synaps-plugin/`). Absolute paths and `..`
components are rejected at install time — the path must resolve inside
the plugin dir.

## What the script must do

1. **Build the extension binary** at the path the manifest's
   `extension.command` points to. For `local-voice` that means
   `bin/synaps-voice-plugin`.
2. **Validate any preconditions** (Rust toolchain, native libs, model
   files). Fail loudly with a non-zero exit on missing prerequisites
   so the install message is actionable.
3. **Be idempotent** — Synaps may call it again on update / refresh.
   Skip work that's already done where you can.
4. **Be quiet on success and verbose on failure** — output is captured
   to a log; users see the log path on failure.

A minimal `scripts/setup.sh` skeleton:

```bash
#!/usr/bin/env bash
set -euo pipefail

plugin_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary_name="my-extension"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: Rust/Cargo is required. Install from https://rustup.rs/" >&2
  exit 1
fi

cd "$plugin_dir"
echo "Building $binary_name..."
cargo build --release

mkdir -p "$plugin_dir/bin"
cp "$plugin_dir/target/release/$binary_name" "$plugin_dir/bin/$binary_name"
chmod +x "$plugin_dir/bin/$binary_name"

echo "Installed: $plugin_dir/bin/$binary_name"
```

## Runtime contract

Synaps invokes the script as:

```
bash <resolved-script>
```

with:

- **cwd** set to the plugin root
- **stdin** closed (`/dev/null`)
- **stdout + stderr** captured to
  `~/.synaps-cli/logs/install/<plugin>-<timestamp>.log`
- **wall-clock timeout** of 10 minutes (`SETUP_TIMEOUT`)

Exit `0` → the install succeeds silently and Synaps reloads the
extension registry. Any non-zero exit, kill-by-timeout, or I/O failure
is surfaced to the user as:

```
installed 'my-plugin' but setup script failed: <error>; see <log-path>
```

The plugin **stays installed** even when setup fails — the source is
on disk, and the user can re-run the script manually. Synaps will
attempt to start the extension on next session; if the build is still
missing, the spawn-error message will helpfully include the exact
command to run:

```
Extension binary missing — this plugin ships source only.
Build it with: (cd /home/u/.synaps-cli/plugins/my-plugin && bash scripts/setup.sh);
then reload.
```

## Security model

Setup scripts are arbitrary shell — they can do anything the user can
do. Mitigations:

- **Path validation**: the declared setup path is canonicalized and
  must live inside the plugin dir (no symlink-escape).
- **Trust gate**: the existing pre-install confirmation dialog (shown
  for any plugin that declares an `extension` block) applies — by the
  time the user has accepted "this plugin will run code on my
  machine," running the build script is strictly less powerful than
  running the extension itself.
- **Wall-clock cap**: a runaway `setup.sh` can't wedge the install
  flow indefinitely.
- **Capture, don't swallow**: every byte of stdout/stderr lands in the
  log. Failed installs are diagnosable.

## Platform notes

- v1 supports POSIX shells only (`bash`). Plugins targeting Windows
  should ship a pre-built binary committed to the repo, or a
  `setup.ps1` shim invoked from a `setup.sh` `case "$(uname)"` block.
- The script inherits the user's `PATH`. If your build needs a
  specific toolchain (`cmake`, `clang`, native libs), the script
  should `command -v` them and fail with a clear message rather than
  proceeding to a confusing rustc error.

## Disabling auto-run

There is no per-plugin "skip setup" flag in v1. If a user wants to
inspect the script before letting it run, they can:

1. Cancel the install at the confirmation dialog
2. Clone the repo manually, inspect the script
3. Drop the cloned tree into `~/.synaps-cli/plugins/<name>/` and run
   the script themselves

In a future iteration we may add `--skip-setup` to the install action
or surface a "show setup script before running" preview pane.

## Related

- `src/skills/post_install.rs` — the helper module
- `src/chatui/plugins/actions.rs` — install-flow wiring
- `src/extensions/manager.rs::compute_extension_load_hint` — runtime
  hint when binary is missing
- [`docs/extensions/README.md`](./extensions/README.md) — what
  extensions are and how they hook into Synaps
