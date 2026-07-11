# SynapsCLI Stability & Compatibility Policy

This document is the authoritative statement of what SynapsCLI promises to keep
stable, what it explicitly does not, and how change is managed when it is
unavoidable. It exists so that extension authors, config maintainers, and
downstream integrators can build against SynapsCLI with a clear understanding of
which surfaces are load-bearing and which are reference implementation.

SynapsCLI is a **runtime with an app-quality floor**: the extension, tool, and
runtime API is a supported, versioned surface; the terminal UI (TUI) is a
polished reference implementation, not a frozen contract. The sections below
make that distinction precise and binding.

---

## 1. Extension Protocol Compatibility Promise

The extension protocol is versioned by a single integer anchor:
`extension_protocol_version`, declared in
[`docs/extensions/contract.json`](./extensions/contract.json) (currently `1`).
This field — not the SynapsCLI release version — is the authoritative version of
the JSON-RPC protocol spoken between the runtime and an extension process. The
same number is echoed to extensions in the `initialize` request
(`params.extension_protocol_version`) and asserted by the extension in its
`initialize` response (`result.protocol_version`), as described in
[`protocol.md`](./extensions/protocol.md).

The compatibility guarantee is:

- **Within a single protocol version, the protocol is stable.** For as long as
  `extension_protocol_version` remains `1`, an extension that correctly speaks
  version 1 will continue to load and run across all SynapsCLI minor and patch
  releases. We will not remove hooks, permissions, methods, actions, or message
  fields, nor change their meaning, within a protocol version.
- **Additive changes are allowed within a version.** New optional hooks, new
  actions, new optional message fields, and new permissions may be introduced
  without bumping `extension_protocol_version`, provided they do not break a
  conforming version-1 extension. Extensions must therefore tolerate unknown
  fields and treat unrecognized optional data as inert — the protocol reserves
  the right to add fields that older extensions ignore.
- **Breaking changes require a version bump.** Any change that could break a
  conforming extension — removing or renaming a hook, changing an action's
  semantics, making an optional field required, or altering the framing — is a
  breaking change and requires incrementing `extension_protocol_version`.
- **Version negotiation fails closed.** If an extension responds to `initialize`
  with a `protocol_version` the runtime does not support, the runtime refuses to
  load it and reports the load failure. Extensions are never run against a
  protocol version they did not agree to.

The machine-readable surface of the current protocol version — hooks, actions,
permissions, matchers, config rules, and reserved names — is enumerated in
`contract.json`, which is drift-checked against the engine in CI. `contract.json`
is the source of truth; this document describes the policy that governs it.

---

## 2. Config-Format Stability & Migration Policy

SynapsCLI treats user and project configuration as a stable interface.

- **Config keys are stable.** Once a configuration key ships in a release, it
  keeps its name, location, and meaning within a major version. This includes
  extension configuration keys — the `extension.<plugin-id>.<key>` user-config
  namespace and the `SYNAPS_EXTENSION_<PLUGIN_ID>_<KEY>` environment overrides
  documented in [`protocol.md`](./extensions/protocol.md) resolve the same way
  across minor releases.
- **Additions are safe.** New optional config keys may be introduced in any
  minor release. New keys must have a documented default that preserves prior
  behavior when the key is absent, so that upgrading without editing config never
  changes how an existing setup behaves.
- **Unknown keys are tolerated, not silently repurposed.** A key that is not yet
  recognized (for example, one written by a newer SynapsCLI) is preserved rather
  than discarded, so that downgrading and re-upgrading does not lose
  configuration.
- **Breaking config changes are migrated, not dropped on the user.** When a
  config key must be renamed, moved, or restructured, SynapsCLI ships an
  automatic migration that reads the old form and rewrites it to the new form,
  and continues to accept the old form (with a deprecation warning — see §3) for
  at least one minor release before the old form is removed. Migrations are
  written to be idempotent and to leave a recoverable prior state where a file is
  rewritten in place.
- **Secrets are never migrated into plaintext.** Migration never copies a value
  resolved from a `secret_env` source into a stored `default` or on-disk config
  value.

---

## 3. Deprecation Policy

Nothing that is part of a stable surface — a protocol hook or action, a
permission, a config key, or a documented CLI flag — is removed without a
deprecation window.

- **Minimum one minor release of warning.** A feature marked deprecated in a
  minor release remains functional for at least that entire minor release before
  it may be removed. Removal therefore happens no earlier than the following
  minor release, and only after the deprecation has been visible to users.
- **Deprecations are announced and observable.** Every deprecation is recorded in
  `CHANGELOG.md` and, where the deprecated surface is exercised at runtime,
  emits a warning through SynapsCLI tracing that names the deprecated item and
  its replacement.
- **A replacement path exists before removal.** A surface is not deprecated until
  its successor is available, so that authors always have a supported migration
  target during the deprecation window.
- **Protocol removals imply a version bump.** Removing a hook, action, or
  permission from the extension protocol is a breaking change and follows §1: it
  is announced as deprecated within a protocol version and only removed under a
  new `extension_protocol_version`.
- **Reserved names are not a deprecation surface.** Names explicitly reserved and
  rejected today (for example `tools.override` in `contract.json`) are not yet
  part of the stable API; activating a reserved name is an addition, not a
  breaking change.

---

## 4. Layer Statement — What Is Stable and What Is Not

SynapsCLI is a runtime first. The commitment below is deliberate and is the
positioning this project stands behind.

**Stable, versioned surfaces (supported for integration):**

- The **extension protocol** — the JSON-RPC contract in `contract.json`, versioned
  by `extension_protocol_version`, covered by §1.
- The **tool and runtime API** — the tool-registration contract, hook semantics,
  permission model, and the runtime's fail-open/timeout behavior that extensions
  depend on.
- The **configuration format** — covered by §2.
- **Documented CLI commands and flags** — subject to the deprecation policy in §3.

These surfaces are what you build against. They change only under the rules
above, and breaking them requires an explicit version bump and a deprecation
window.

**Explicitly unstable (a reference implementation, not a frozen surface):**

- The **TUI internals** — layout, widget structure, key bindings, rendering
  details, styling, panels, and every other aspect of the terminal interface.

The TUI is held to an app-quality floor: it is meant to be genuinely good to use.
But that polish is a *reference implementation of the runtime*, not a stability
contract. TUI internals may change in any release without a deprecation window,
and integrators must not scrape, screen-parse, or otherwise couple to the TUI's
presentation as if it were an API. When you need a stable interface, use the
extension/tool/runtime API and the config format — that is what "stable and
versioned" applies to. The TUI is the app quality on top of the runtime; the
runtime is the promise.

---

## Summary

| Surface | Stability | Governed by |
|---|---|---|
| Extension protocol (`extension_protocol_version`) | Stable within a version; breaking changes bump the version | §1 |
| Tool / runtime API | Stable, versioned with the protocol | §1, §4 |
| Config format & keys | Stable within a major; breaking changes are migrated | §2 |
| Documented CLI commands/flags | Stable; removals follow deprecation window | §3 |
| Deprecations | Minimum one minor release of warning before removal | §3 |
| TUI internals | **Explicitly unstable** — reference implementation | §4 |
