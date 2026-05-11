# Smoke: watcher → bridge UDS heartbeat mirror

This playbook verifies that the SynapsCLI watcher mirrors per-agent
heartbeats over the bridge daemon's `ControlSocket` (`heartbeat_emit`
op) when `[bridge].heartbeat_mirror` is enabled.

## Prereqs

- Bridge daemon (synaps-skills, Phase 5+) running and exposing a UDS
  control socket. Default path: `~/.synaps-cli/bridge/control.sock`.
- At least one watcher agent configured under `~/.synaps-cli/watcher/<name>/`.
- `socat` installed (for the manual UDS probe in step 4).

## Steps

### 1. Enable the mirror

Edit `~/.config/synaps/config.toml` (or the file referenced by
`SYNAPS_CONFIG`) and add:

```toml
[bridge]
heartbeat_mirror = true
# Optional overrides:
# uds_path = "/custom/path/control.sock"
# heartbeat_timeout_ms = 250
```

When unset, `uds_path` resolves to `<base_dir>/bridge/control.sock`
(usually `~/.synaps-cli/bridge/control.sock`).

### 2. Start the watcher supervisor

```bash
synaps watcher start
```

You should see this line in the watcher log within the first second:

```
[watcher] bridge heartbeat mirror ENABLED (uds=…/control.sock, timeout=250ms)
```

### 3. Confirm bridge sees per-agent heartbeats

Within ~30 s of any agent posting a heartbeat (default
`interval_secs = 30`), the bridge `/health` endpoint should report a
component entry per agent. With the bridge daemon's HTTP probe:

```bash
curl -s http://127.0.0.1:<bridge-port>/health | jq '.components[] | select(.component=="agent")'
```

Expected: one object per running watcher agent, with `healthy: true`
and a `lastSeen` timestamp newer than the watcher's heartbeat
interval.

### 4. (Optional) Manual UDS probe

Send a one-off `heartbeat_emit` directly to the bridge to validate
framing without involving the watcher:

```bash
echo '{"op":"heartbeat_emit","component":"agent","id":"smoke-test","healthy":true,"details":{},"synaps_user_id":"local"}' \
  | socat - UNIX-CONNECT:$HOME/.synaps-cli/bridge/control.sock
```

Expected response (one line):

```json
{"ok":true,"ts":"2025-…"}
```

### 5. Verify graceful degradation

Stop the bridge daemon and watch the SynapsCLI log with
`RUST_LOG=watcher::bridge=debug`. The watcher must keep running.
You should see lines like:

```
DEBUG watcher::bridge: bridge heartbeat mirror failed (non-fatal) agent=… error=bridge socket unavailable: …
```

Agents must continue to spawn, restart, and heartbeat normally — the
bridge being offline is never fatal.

## Tear-down

Set `heartbeat_mirror = false` (or remove the `[bridge]` block) and
restart the watcher to disable mirroring.
