# Changelog

All notable changes to this project will be documented in this file.

## [0.8.0] — 2026-08-04

### Added

- **`myx` theme — album-reactive colors via MXC.** Synaps now ships a builtin
  theme that subscribes to the Myx Color Protocol socket
  (`$XDG_RUNTIME_DIR/myx/theme.sock`) while active: every track change in the
  Myx music player re-derives a 16-token palette from the album art and Synaps
  recolors itself live. Fully functional without Myx installed (static
  tokyonight-based snapshot); works across machines over an SSH Unix-socket
  forward. The subscriber can never hurt the session: capped-backoff reconnect,
  keep-last-good on disconnect, bounded line reads, and a socket-ownership
  check on the `/tmp` fallback.
- **Animated theme transitions.** Theme changes — `/theme`, settings, and live
  MXC palettes — now cross-fade with ease-in-out easing instead of snapping.
  New `theme_transition = on | off | <ms>` config knob (default 350ms, also in
  the settings modal); MXC's per-message `fade_ms` is honored. Transitions ride
  the existing animation tick: zero idle cost, byte-exact landing.
- **Real input editing (tui-textarea).** The input editor now runs on a
  maintained editing core: proper cursor movement and selection behavior,
  Ctrl-Z/Ctrl-Y undo/redo, and Alt-Enter to insert a newline in any terminal.

### Performance

- **Prompt-cache fix: per-turn context no longer rewrites your history.**
  Extension-injected per-message context (memory, tasks, timestamps) was
  appended to the system prompt — mutating the cache prefix and forcing the
  entire conversation history to be re-uploaded at write pricing on every user
  turn. Injection now rides the newest user message after the cache
  breakpoint: measured per-turn cache writes dropped from ~97K tokens to ~13
  in long sessions, with 99–100% cache hits. Long-running sessions are
  dramatically cheaper.

### Fixed

- **Extension protocol:** `on_session_start` is delivered (and can inject
  session context); the protocol contract is drift-checked against the engine
  itself; string JSON-RPC request ids accepted; capability ids validated with
  the runtime's own validator.
- **Multiline paste:** terminals/tmux converting `\n` to `\r` in bracketed
  paste no longer collapses pasted text onto one line.
- **tmux rendering:** transcript-canvas edges no longer paint the wrong color;
  invisible transcript canvas preserved under tmux.

### Changed

- **Theme polish wave:** refined default surface hierarchy, user turns lifted
  onto chrome (default + ocean), hand-picked conversation canvas for every
  builtin theme, and more breathing room between turns.


## [0.7.1] — 2026-08-01

### Added

- **Kimi Code OAuth provider (device flow).** New OAuth device-flow login for
  Moonshot Kimi Code, complementing the static-key `kimi` provider shipped in
  0.7.0. Run `synaps login --provider kimi-code`, complete the device
  authorization in your browser, and Synaps routes through the managed Kimi
  Code runtime with broker-owned token refresh. Access tokens never leave the
  broker; the model runtime only sees ephemeral session material.
- **InferX static-key provider.** New `inferx` provider pinned to
  `https://model.inferx.net/endpoints/v1`, discovering the API key from
  `INFERX_API_KEY` or `synaps login --provider inferx`. Default model is
  `deepseek-v4-flash`; other InferX-published model IDs are usable as
  `inferx/<model-id>` and surface through the standard OpenAI-compatible
  `/models` catalog fetch.
- **Themed conversation canvas.** Every built-in palette now specifies a
  distinct `message_bg` for the transcript area, giving code blocks and user
  cards a real elevation instead of blending into the chrome. The new
  `background` setting toggles the canvas between the themed color
  (**opaque**) and the terminal-transparent behaviour of 0.7.0
  (**invisible**), scoped strictly to the conversation area — header, input,
  footer, and modal chrome are unaffected either way.

### Fixed

- **Background toggle no longer bleeds into chrome.** An earlier iteration
  routed the toggle through every surface, dimming the header/footer when
  set to invisible. The toggle now owns exactly one surface: the conversation
  canvas.

### Changed

- **quinn-proto bumped to 0.11.15** (transitive; picked up via `cargo check`
  during release prep). No API impact.

## [0.7.0] — 2026-07-25

### Added

- **Kimi (Moonshot AI) static-key provider.** New `kimi` provider pinned to
  `https://api.moonshot.ai/v1` with broker-owned key discovery from
  `MOONSHOT_API_KEY` / `KIMI_API_KEY`. Catalog covers Kimi K2.7 Code
  (default), K2.7 Code Highspeed, K3 (1M context), and K2.6 — all
  OpenAI-compatible with tool calling and reasoning support. Use models as
  `kimi/<model-id>`. `/effort` exposes exact documented reasoning modes per
  model: K3 supports `low` / `high` / `max` (default `max`) via the
  top-level `reasoning_effort` field; K2.6/K2.5 support only a thinking
  on/off toggle (`thinking.type`); the K2.7 Code family always thinks with
  no effort control. Unsupported levels are rejected at mutation time —
  never silently swallowed by the API.

- **Typed, brokered provider authentication and routing.** `synaps login` now
  discovers typed OAuth, cloud, and static-key targets from provider
  descriptors. New broker-owned flows cover Grok/xAI, GitHub Copilot, Google
  Gemini CLI / Code Assist, Azure OpenAI, Amazon Bedrock, and Google Vertex AI,
  while preserving Anthropic and OpenAI Codex. Provider-qualified identities
  survive login, persistence, model selection, catalog refresh, runtime routing,
  and status output without exposing long-lived credentials to model runtimes.
- **Exact model capabilities and live effort controls.** `/settings` reuses the
  model picker and `/effort` exposes only modes authorized for the active exact
  model. OpenAI Codex keeps seven source-controlled ChatGPT OAuth model rows and
  distinct `xhigh`, `max`, and logical `ultra` modes. Claude Fable 5 adds wire
  `max` and logical `ultracode`; UltraCode lowers to Anthropic `xhigh` plus
  bounded orchestration doctrine rather than sending a made-up wire value.
- **Typed modular prompt orchestration.** Source-controlled manifests compose
  exact provider/model/role prompt modules, path-confine local files, persist
  provenance, reload transactionally, and install runtime-owned delegation
  policy. Worker authorization validates exact models, roles, write scopes,
  grants, concurrency, and recursion before credentials, network, billing,
  threads, or worker-state side effects.
- **Reactive subagent lifecycle tools and completion gates.** Foregrounds can
  start, poll, steer, inspect, collect, resume, and explicitly reconcile
  workers. Completion waits for required reconciliation. Manifestless
  OpenRouter sessions have an exact three-model worker authority for DeepSeek
  V4 Pro, GLM 5.2, and Kimi K2.7 Code; other manifestless providers remain
  foreground-only unless explicitly authorized.
- **Brokered dynamic catalogs and provider-native streaming.** Catalog and
  streaming support now covers xAI Responses, GitHub Copilot descriptors,
  Gemini Code Assist, Azure OpenAI, Bedrock ConverseStream, Vertex SSE, OpenAI
  Codex, and Anthropic. Broker requests pin hosts and paths, reject redirects,
  and retain credentials inside the broker boundary.

### Changed

- Model identity is provider-qualified end to end; model switching now replaces
  manifestless orchestration authority atomically and is refused while workers
  still require collection or reconciliation.
- OpenAI-compatible schema translation recursively sanitizes tool schemas and
  preserves provider-specific call identifiers, while Gemini function
  responses use Code Assist-compatible object payloads and replay thought
  signatures.
- Direct Codex transport errors include provider hosts and full cause chains,
  retry only bounded transient send failures, and use idle read timeouts so
  healthy long-running streams can continue.

### Fixed

- **Reactive subagent wake and garbage collection.** Finished workers no longer
  disappear before collection, and expired unreconciled workers become bounded,
  transport-free tombstones. Tombstones release steering/shutdown channels,
  receiver/thread state, tool logs, and resumable conversation state while
  retaining a UTF-8-safe terminal-output tail and idempotent
  collection/reconciliation path. Reconciled tombstones are removed on the next
  reaper pass, eliminating the completion-gate ⇄ GC deadlock.
- Gemini Code Assist continuation now honors rate-limit reset hints, serializes
  function responses correctly, and removes a public-API alias that Code Assist
  rejects.
- TUI model and effort changes fail closed on unsupported provider/model/mode
  combinations, stale capability generations, active streams, and
  unreconciled-worker races without persisting partial state.

### Testing

- Added zero-network OAuth, broker, cloud, catalog, routing, reasoning,
  orchestration, provider-confinement, and streaming harnesses; golden request
  bodies for OpenAI and Anthropic; and an external tombstone harness proving
  bounded allocation release and real completion-gate recovery.
- Live certification artifacts exercised Grok 4.5 fan-out, GLM 5.2 recovery,
  and a heterogeneous DeepSeek → GLM → Kimi workflow. These are validation
  evidence, not guarantees of upstream entitlement or availability.

### Fixed (existing development line)

- **Reactive subagent wake (Phase 1)** — subagents no longer silently vanish
  when the parent turn ends before they complete. `cleanup_finished()` now
  retains uncollected finished handles for 15 minutes (TTL reaper); a
  `finalize_subagent` hook fires exactly once at each subagent thread exit
  (outside `catch_unwind`), stamps `finished_at`, and pushes a
  `subagent_completion` event to the parent's `EventQueue` so the idle parent
  wakes and can call `subagent_collect`. Kill-switch:
  `SYNAPS_DISABLE_SUBAGENT_WAKE=1`. Covers start, resume, panic, timeout, and
  tokio-build-failure exit paths. Encodes the live regression as integration
  test I1 (`parent_wakes_after_turn_end_multi_subagent`). See
  `IMPLEMENTATION-PLAN.md` in the workspace for full design rationale.
## [0.6.1] — 2026-07-18

### Added

- **Reactive subagents** — full lifecycle support for `subagent_start`,
  `subagent_collect`, `subagent_resume`, `subagent_steer`, and
  `subagent_status`. Subagents now publish a `subagent_completion` event to the
  parent's `EventQueue` at thread exit (via a single terminal finalizer outside
  `catch_unwind`), waking idle parents automatically. Finished-uncollected
  handles are retained for 15 min via a TTL reaper. Kill-switch:
  `SYNAPS_DISABLE_SUBAGENT_WAKE=1`. Covers start, resume, panic, timeout, and
  tokio-build-failure exit paths.
- **Runtime event system** — canonical `RuntimeEvent` frames added to the
  protocol layer; the engine centralizes event drain and wake policy; the TUI
  persists the subagent HUD across turns via registry reconciliation;
  cross-mode event continuation contract enforced with lint gates and
  integration tests.
- **Event reactor** — sessions broadcast runtime events with policy-gated
  continuation; RPC mode forwards events without auto-turning; chat mode wakes
  and continues turns on injected runtime events.
- **Auto-turn continuation** — agent reacts to runtime events instead of
  polling; auto-turn cap boundary enforced atomically; cross-mode races closed.

### Fixed

- **Extension secrets retained across restart (PR #64)** — resolved config
  (including user-supplied secrets) is now preserved when an extension process
  restarts, eliminating repeated secret prompts.
- **RPC socket path length fix (PR #64)** — runtime socket root shortened to
  stay within the OS 104-char `sun_path` limit; path revalidation added on
  client registration.
- **Prompt-injection stripping** — tab whitespace stripped from client prompts
  before registration to block injection via indentation.
- **Shutdown zombie stream timeout** — auto-turn reservations are not made on
  terminal error paths; abort is honored across auto-turn registration;
  recursive futures eliminated in auto-turn scheduling.

## [0.6.0] — 2026-07-11

The TUI modernization release. 112 commits, 6 waves of architectural work, two
major performance rewrites, and a complete modal/focus system overhaul.

### Added

- **ModalStack + FocusManager** — proper modal/pane lifecycle with stack-based
  routing, eliminating ad-hoc boolean flags (P7, 8 slices).
- **TuiClock** — deterministic time source for testable animations (P6.2).
- **Interaction tapes** — JSON record/replay for deterministic UI test
  scenarios (P6.4).
- **Headless test harness** — drives the real TUI dispatch surface without a
  terminal; bounded async executor for streaming scenarios (P6.1–P6.5).
- **TermCaps** — terminal capability detection via DA1 queries; gates
  edge-scrub, sync-output, and Kitty graphics on actual terminal facts (P16).
- **Namespaced extension theme tokens** — `ext.<id>.<token>` for per-plugin
  theming with per-part chrome overrides (P19).
- Discord invite link in README.

### Performance

- **Arc-share conversation messages (#128)** — per-turn history copy overhead
  reduced from 4,536 KB to 0 KB (−100%). Every history clone is now a pointer
  bump. 13 golden byte-gate fixtures protect prompt-cache prefixes.
- **Lazy transcript virtualization (#241)** — cold-resume renders reduced from
  1,000 to 21 (−97.9%), syntax highlights from 200 to 4 (−98.0%), first-frame
  RSS from 14.2 to 10.6 MB (−25.4%). ScrollAnchor + promote-on-touch model.
- **Combined real-world result:** 70.8 → 30.7 MB PSS (−57%) on a 1,189-message
  session.

### Changed

- **run() split (P12)** — `tui/mod.rs` reduced from 2,597 to 443 lines.
  Boot prologue → `run_setup.rs`, input dispatch → `dispatch.rs`, stream
  handlers → `stream_handler.rs`.
- **ViewInputs seam (T199.2)** — render-input boundary extracted, reconciling
  the view-model split after P7/P12.
- Secret-prompt handling folded into ModalStack (P7.8).

### Fixed

- Plugin-category settings panic + category-to-index mis-map (PR#60 review).
- Abort-path mutex ordering fix (PR#60 review).
- 18 pre-existing clippy lints resolved (#240).

## [0.5.1] — 2026-07-03

### Added
- **`synaps auth-broker` now supports TLS (HTTPS) via `--tls-cert`/`--tls-key` PEM flags.**
  When both flags are provided the broker binds with axum-server + rustls (rustls 0.23,
  no OpenSSL dependency) instead of plain HTTP.  A new bind-policy enforcer refuses to
  start on non-loopback addresses without either TLS or an explicit `--insecure-http`
  acknowledgement; the latter prints WireGuard-only guidance.  Startup validation parses
  the cert/key PEM before binding and fails fast with a readable error on bad files.
  New tests cover: policy matrix (7 cases), PEM validation (garbage cert, garbage key,
  cert-as-key, valid pair), file-path loading, and a real HTTPS integration suite
  (rcgen self-signed CA, reqwest with `danger_accept_invalid_certs`) checking /healthz
  and machine-auth enforcement over TLS.  (#156 subtask 6)
- **The runtime now retries when the model stops with `stop_reason=refusal`, up to a
  configurable bounded budget.** Previously a refusal terminated the turn silently —
  the user saw nothing and the agent stalled. The retry path resubmits the request
  with an escalating context nudge, caps attempts to avoid runaway spend, and surfaces
  the final refusal explicitly in the TUI if all retries are exhausted. Retry count and
  budget ceiling are tunable in config. (#188)

### Fixed
- **SECURITY — Two Anthropic OAuth vulnerabilities patched.**
  - **CSRF bypass in manual-paste login.** `parse_manual_input` previously accepted a
    bare authorization code (no `state` parameter) and the login task fell back to
    comparing `manual_state` against itself, silently nullifying the CSRF check. The
    bare-code path is removed; `manual_paste_to_callback` now returns `Some` only when
    both `code` and `state` are present in the pasted URL. Six unit tests cover the
    accept/reject matrix including bare codes, missing-state URLs, and malformed input.
  - **Non-atomic write in `ensure_fresh_token` could zero `auth.json`.** The old path
    held an `fs4` exclusive lock on the live file, truncated it in place, then wrote
    the new token — a crash between truncate and write completion left an empty
    `auth.json`. Because Anthropic rotates the refresh token on every refresh call,
    the zeroed state is unrecoverable. The fix: open `auth.json` read-only, drop the
    lock, perform the network refresh, then delegate the write to
    `save_provider_auth_at` in `storage.rs`, which uses a
    `auth.json.lock → auth.json.tmp → rename(2)` sequence (atomic on POSIX). A crash
    at any point leaves the pre-refresh credential intact. As a side effect this also
    fixes silent provider-data loss (#184): the old `AuthFile` reconstruction silently
    dropped any provider beyond `anthropic`/`openai_codex`; the read-merge-write path
    in `save_provider_auth_at` preserves all providers. Four regression tests added.
- **`login --provider` no longer wipes other providers' credentials.** The old
  `save_auth` whole-file overwrite dropped every provider not included in the current
  write. The bug was fixed by the `agent-core` extraction refactor; a regression suite
  (codex-then-anthropic ordering, reverse ordering, third-provider preservation,
  same-key upsert) has been added and verified RED against the pre-refactor
  implementation. (#159)
- **Spawned subagents now reliably enforce the 5-minute cache TTL policy.** Previously
  the three subagent spawn paths (oneshot / start / resume) only called
  `apply_auth_config`, so the 5m TTL was inherited by accident from `Runtime::new()`
  defaults — any change to the parent's cache config could silently propagate to
  short-lived spawns (~$0.23 wasted per 10-spawn fan-out on long-TTL parent configs).
  A new `apply_subagent_runtime_policy()` function is the single enforcement point
  called in all three paths, decoupled from whatever the parent runtime has set. (#110)
- **Extension tools are no longer missing on turn 1 in headless/pipe mode.** When
  input was piped to `synaps chat` (e.g. `echo "…" | synaps chat`), stdin was
  immediately ready and the first API request could fire ~350ms before extension
  processes finished registering their tools — the model never saw extension-provided
  tools on that turn. `discover_and_load()` is now awaited before the read loop starts,
  so every extension's tools are in the registry before any message reaches the API.
  The interactive TUI is unaffected. (Found while containerizing a single-tool agent.)
- **Three TUI safety bugs from the safety review.**
  - **Busy-spin on zero-size or erroring terminal** (HIGH). A `0×0` or `Err` return
    from `terminal_size()` hit a `continue` branch that skipped clearing
    `needs_redraw`/`force_redraw` and updating `last_draw`, causing the event loop to
    re-enter `should_draw()` at full CPU speed indefinitely. Both flags are now cleared
    and `last_draw` bumped before the `continue`.
  - **Char-boundary panic in `mask_key`** (MED). API keys containing non-ASCII
    characters (accented chars, emoji) caused a byte-index slice on the render thread
    to panic. `mask_key` now collects `chars()` and indexes by character position.
  - **Modal width overflow on narrow terminals** (MED). Fixed-minimum clamps on the
    secret-prompt modal, plugin editor, and picker could produce a modal wider than
    `area.width`. Each site now caps its lower bound to the available width before
    layout. A width-exhaustive test (0–200 columns) guards all three sites.
- **Tool-execution error messages no longer repeat "Tool execution failed:" twice.** Five
  call sites in `runtime/stream.rs` and `runtime/mod.rs` were wrapping errors that
  already carry the prefix via `RuntimeError::Tool`'s `Display` impl, producing doubled
  output like *"Tool execution failed: Tool execution failed: …"* in the TUI. The
  redundant format strings are removed; the TUI's `starts_with` sentinel is unaffected.
- **Stale truncation test fixed and `MAX_HOOK_OUTPUT` promoted to a public constant.**
  `after_tool_call_truncates_utf8_safely` was testing against the old 32 KiB cap after
  it was raised to 256 KiB — truncation never fired, leaving the test permanently green
  for the wrong reason. The constant is now `pub const MAX_HOOK_OUTPUT` in `events.rs`
  so tests anchor to the real value. The fixture is rewritten to straddle the cap with
  a 2-byte UTF-8 sequence and also asserts the leading slice is valid UTF-8 (proving the
  boundary logic never cuts through a multibyte char).
- **`TriggerConfig::default()` now correctly sets `debounce_secs = 3`.** `#[derive(Default)]`
  produced `debounce_secs = 0` (`u64::default()`), ignoring the `default_debounce()`
  function wired to the `serde` attribute. A TOML with no `[trigger]` table therefore
  got zero debounce, causing the file watcher to fire on every partial write instead of
  settling for 3 seconds. `Default` is now hand-written, matching the pattern already
  used by `SessionLimits`, `BootConfig`, and `HeartbeatConfig` in the same file.
- **Multibyte characters no longer panic the error humanizer.** `&trimmed[..200]`
  panicked when an emoji, CJK character, or smart quote straddled byte offset 200.
  The slice is replaced with `truncate_str(trimmed, 200)`, which walks back to the
  nearest valid char boundary before cutting.

### Testing
- **Integration tests for on-401 broker-token refetch added.** Three `wiremock`-style
  `axum` tests (via a new `SYNAPS_ANTHROPIC_BASE_URL` test seam in `ApiOptions`) cover:
  remote credential source retries once on 401 and succeeds; persistent 401 is terminal
  after exactly two upstream calls (no infinite loop); local credential source does not
  retry on 401. RED-GREEN verified against the live guard in `call_api_stream_inner`.
  (#157 subtask 9)

## [0.5.0] — 2026-06-30

### Added
- **Extensions can rewrite tool output before it enters history (`HookResult::Replace`).**
  An `after_tool_call` hook may now return a `Replace` action that substitutes the
  tool's output before it's appended to the conversation — enabling context
  virtualization, redaction, summarization, or any transform of what the model
  sees, implemented as an extension rather than baked into the core. Honored at
  **all 5** `after_tool_call` sites for consistent behavior across every tool path.
  - **Security — two-key gate.** Rewriting tool output is privileged: an extension
    must both declare the capability *and* be granted it; a single switch isn't
    enough to silently alter what the model reads. (From security review.)
  - **Audit trail.** Every `Replace` is recorded to a persistent audit log so
    output rewrites are accountable and inspectable after the fact.
  - **Docs.** The `replace` action and `tools.transform_output` are documented
    across the extension docs (hooks, permissions, protocol, contract), with a
    worked `replace_extension.py` fixture proving the path end-to-end.

### Changed
- **Oversized tool output now compresses before it truncates.** When a tool's
  output exceeds the buffer, the runtime first attempts to compress/condense it
  rather than hard-cutting the tail, so more of the useful signal survives instead
  of being lost to a blunt truncation. The preview remains a ~32KB prefix plus a
  marker (clarified in docs — it is a preview, not a hard cap).
- **Unambiguous plugin commands resolve by bare name.** A plugin command with no
  naming collision can be invoked directly (e.g. `/crush`) instead of requiring
  the fully-qualified `/plugin:command` form. Ambiguous names still require
  qualification.

### Internal
- **TUI layout layer hardened.** Migrated all 12 `Layout::split()` sites to the
  typed `.areas()` API (named bindings, compile-checked pane counts — a class of
  runtime index panics becomes compile errors) and extracted the main 6-pane
  layout into a single `AppAreas` function, removing duplicated height math that
  had to be kept in sync by hand. No behavior change — equivalence verified live.

## [0.4.0] — 2026-06-29

### Added
- **Shared credentials via a broker (`auth.remote_endpoint` + `synaps auth-broker`).**
  Many machines share ONE OAuth credential without copying it to each disk — both
  **Anthropic and OpenAI/codex** tokens route through the broker. On a client, set
  `auth.remote_endpoint` (+ `auth.machine_token`, or env `SYNAPS_AUTH_ENDPOINT` /
  `SYNAPS_MACHINE_TOKEN`) and it fetches short-lived access tokens from a broker
  instead of reading/refreshing the local `auth.json`. Clients hold only access
  tokens in memory — never a refresh token, never written to disk; the broker is
  the single refresher. Run the broker with `synaps auth-broker` on a trusted
  host (`--machine-token-file`, behind WireGuard or a TLS-terminating proxy).
  Closes the agentic-VM bootstrap: guests authenticate with zero credentials on
  their own disk.
  - **Security:** constant-time machine-token compare, provider allowlist,
    in-flight rate cap, refuses unauthenticated non-loopback start, graceful
    shutdown, redacting `Debug` on secrets, on-401 refetch+retry, clock-skew
    defense (`ttl_ms`), token validation, and a `systemd` unit at
    `deploy/synaps-auth-broker.service`.
  - **ToS:** intended for sharing one account across YOUR OWN machines. Sharing a
    subscription seat across users / a large fleet risks account suspension — use
    org/enterprise API keys at scale. See the README.

### Fixed
- **Flaky test isolation in `synaps-tui` (#137).** Config-env tests (config
  migration + plugin actions) raced on `SYNAPS_BASE_DIR`/`HOME` through two
  independent locks, causing intermittent failures in parallel `cargo test` runs
  only. Unified all config-env tests under one shared test lock; verified green
  across repeated parallel runs.

## [0.3.10] — 2026-06-24

### Added
- **Non-interactive provider login (`auth login --provider`)** — adds a `synaps auth login` subcommand with a `--provider <id>` flag for scripted/headless OAuth and API-key login. It dispatches to the existing provider handlers (e.g. `openai-codex` via `login_openai_codex`), so a login can be driven end-to-end without a TTY prompt. `synaps login` still runs interactively when no provider is given. This unblocks headless bootstrap flows (e.g. an agentic-VM guest that has to authenticate without a human at the keyboard). Covered by 8 unit tests in `cmd::login` (provider lookup, claude-first ordering, openai-codex resolution, relaunch arg/profile handling).

## [0.3.9] — 2026-06-23

### Added
- **Disable built-in tools via config (`disabled_tools`)** — add `disabled_tools = bash, ls` (comma-separated runtime names) to `~/.synaps-cli/config` to remove specific built-in tools from the registry at boot, so they're never offered to the model. Useful for locking down what an agent can do (e.g. a read-only or no-shell profile). Mirrors the existing `disabled_plugins` / `disabled_skills` options; unknown names are ignored.

### Changed
- **TLS now trusts both bundled *and* system CA roots** — 0.3.8 switched to bundled Mozilla roots (webpki) so HTTPS works on minimal systems, but that dropped pickup of custom/corporate CAs installed only in the OS trust store. HTTPS now trusts the *union*: the compiled-in Mozilla bundle **and** the system trust store. Bare containers still work (bundled roots), and enterprise/proxy/custom CAs are honored again (system roots). Both root sources are loaded explicitly on every HTTP client, so the behavior doesn't depend on dependency defaults. Verified: still completes TLS in a bare Alpine container with no system CAs, *and* trusts a custom CA present only in the system store.

### Fixed
- **Scroll restored after the casino (`/gamba`)** — returning from the GamblersDen mini-game left the terminal with mouse capture and bracketed paste disabled, so the scroll wheel (and paste) stopped working in the TUI. The terminal handoff now re-enables both on the way back, matching the initial setup.

## [0.3.8] — 2026-06-16

### Added
- **Static Linux binary (`x86_64-unknown-linux-musl`)** — releases now ship a fully static, musl-linked Linux build alongside the existing glibc one. Because musl is linked statically, the binary carries its own libc and depends on *nothing* on the host — it runs on Alpine, `scratch`/distroless containers, busybox, and older distros where a glibc build dies with `GLIBC_2.xx not found` or simply has no libc to link against. Verified end-to-end: the binary boots and completes a TLS handshake inside a bare Alpine container with no glibc and no system CA certificates.

### Changed
- **TLS trust now uses bundled Mozilla CA roots (webpki-roots)** — HTTPS previously trusted the *system* CA store (`rustls-tls-native-roots`), which reads `/etc/ssl/certs` at runtime. Minimal environments (Alpine, slim/scratch containers, bare CI images) often ship without `ca-certificates`, so every HTTPS call failed there with a certificate/"could not reach" error. The CA roots are now compiled into the binary, so HTTPS works out of the box on any system regardless of what's installed. This is what makes the static musl binary genuinely "runs anywhere." (Trade-off: a custom/corporate CA installed only in the system trust store is no longer picked up automatically.)

## [0.3.7] — 2026-06-15

### Added
- **Configurable streaming frame rate (`max_fps`)** — the TUI's streaming redraw rate is now a config knob. Add `max_fps = 144` (or `240`, or whatever your display runs) to `~/.synaps-cli/config`; default is `60`. Until now streaming redraws were hard-capped at ~10fps — a deliberate throttle from when each redraw rebuilt the whole transcript (O(n)). With 0.3.4's incremental cache (O(changed)) and 0.3.6's O(viewport) publish, a streaming frame is cheap, so the cap is now a real, tunable frame rate. Values are validated (1–1000) with a boot warning on bad input; user input (scroll/typing) always redraws immediately regardless of the cap.

## [0.3.6] — 2026-06-15

### Performance
- **Streaming render publish is now O(viewport), not O(transcript)** — each frame, the TUI built its render snapshot by deep-cloning the *entire* line buffer (every message, every span) just so the render thread could slice the ~one screenful it actually draws. On long sessions that per-frame full clone was the last cost standing between streaming and a flat 60fps. The visible window is now sliced on the publishing side, so the snapshot carries only the lines on screen — publish cost no longer grows with session length. Completes the streaming-render perf arc: 0.3.3 throttled rebuild *frequency*, 0.3.4 cut rebuild *cost* (O(changed messages)), and 0.3.6 removes the residual O(n) publish clone. Selection and scrolling are unchanged.

### Fixed
- **Scrolling and typing stayed smooth during streaming** — the 0.3.3 redraw throttle (~10fps during active turns) also throttled *your* input, so scrolling the transcript or typing while the agent streamed felt sluggish. User input now bypasses the throttle (immediate redraw on scroll/keypress) while streaming text stays throttled — responsive controls without giving back the streaming CPU win.

## [0.3.5] — 2026-06-14

### Fixed
- **Shell installer failed on systems without `xz`** — release tarballs shipped only as `.tar.xz`, but minimal environments (slim containers, bare CI images) lack `xz`/`xz-utils`, so extraction died with `xz: Cannot exec`. Release archives are now `.tar.gz` (gzip is universal), and the generated installer extracts with `tar xzf`.
- **`/gamba` did nothing on `.deb` installs** — the Debian package shipped only `/usr/bin/synaps`, omitting the `hidden` companion binary, so the casino command hit its "nothing here" path. The `.deb` now ships both binaries (parity with the tarball, Homebrew, and crates.io channels).

## [0.3.4] — 2026-06-14

### Performance
- **Incremental message render cache** — the TUI now re-renders only the message(s) that actually changed during streaming, instead of rebuilding the entire transcript on every delta. Per-frame render cost drops from O(transcript size) to O(changed messages), so main-thread CPU during streaming stays flat regardless of how long the session runs. Completes the streaming-render perf work begun in 0.3.3: that release throttled rebuild *frequency* (~60fps → ~10fps); this one cuts the *cost per rebuild*. The full-transcript renderer is retained as a test oracle.

## [0.3.3] — 2026-06-14

### Added
- **`night-city` theme** — a premium neon-noir palette (Cyberpunk / Blade Runner / Neuromancer accents on a near-black base). Strictly opt-in via `theme = night-city`.
- **Tool transcript overhaul** — per-tool colored gutter + glyph identity (bash / read / write / edit / grep / find / ls / subagent / ext), subtle per-tool panel backgrounds (input darker, output lighter), paired input→output ordering for parallel tool calls, and an `ok / timed out` status footer under each result. All themeable via new `tool_*` theme keys.

### Performance
- **Streaming redraw throttled ~60fps → ~10fps during active turns** (#131). The per-delta full `RenderModel` rebuild — which scales with session size — was the main-thread CPU cost; the spinner only needs ~10fps and deltas arrive faster than the eye reads. The idle/final frame still renders immediately, so end-of-turn state never lags.

### Fixed
- **Tool colors no longer leak across themes** — `tool_*` accents now derive from each theme's *own* palette (only `night-city` carries the neon set), and the no-config default theme is unchanged. Pick `dracula` and the tool gutters look like dracula, not Night City.
- **Thinking spinner could spin forever** on an empty thinking step — the placeholder is now dropped when the agent produces text/tools or the turn ends. A real model `…` is preserved via a sentinel so it's never mistaken for the placeholder.

## [0.3.2] — 2026-06-14

### Fixed
- **`synaps.log` was silently dead** — after the A3 split the tracing `EnvFilter` only enabled `synaps_cli=debug`, but the code now emits from `agent_core` / `agent_engine` / `agent_tui` targets. With `RUST_LOG` unset (the default) every log line was filtered out. Added the three crate targets to the filter.
- **Version header showed a bogus `<hash>-dirty` on source-tarball installs** — `build.rs` shelled out to `git` and trusted whatever repo it found; AUR/Homebrew extract the source *inside* the package's own git checkout, so it baked an unrelated commit + a spurious `-dirty`. Now it only trusts git when its repo root **is** the synaps workspace root; any packaged build (AUR / Homebrew / crates.io) shows `vX.Y.Z · release` instead. CI-built release binaries still carry the real commit.
- **Empty `end_turn` could misclassify as a silent-stop error in the default (telemetry-off) config** — `has_stop_reason` was derived from a telemetry-gated field. Now captured unconditionally. (#130 follow-up)
- **In-stream API error bodies reached the screen unsanitized** — a hostile/MITM endpoint could inject terminal escape sequences via an `error` event message. Error text is now sanitized like notices.
- **Mid-stream transport drops are now retried** through the unified retry budget instead of hard-failing the turn.
- **`build.rs` `-dirty` no longer triggers on untracked files** (`--untracked-files=no`); SSE line buffer gained an 8 MiB hard cap against a no-newline OOM; telemetry records now carry the real retry attempt number.

### Changed
- **crates.io publish job** runs with verification enabled and keys idempotency off the process exit code (not log grep), so a genuine publish failure fails the release loudly instead of silently skipping a crate.

### Docs
- User + contributor docs (README, AGENTS, in-app `/help`, and the 36-page wiki) cross-checked against the code and refreshed for the 0.2→0.3 / A3 workspace reality.

## [0.3.1] — 2026-06-14

### Fixed
- **`cargo install synaps` from crates.io was broken in 0.3.0** — after the A3 workspace split, `synaps-engine` embedded its help data via `include_str!("../../../assets/help.json")`, a file at the *workspace root, outside the crate*. `cargo publish` only packages files inside the crate, so the published `synaps-engine`/`synaps` 0.3.0 could not compile (`No such file: assets/help.json`). The asset now lives inside the crate at `crates/agent-engine/assets/help.json`, and the crates.io publish job runs **with verification** (no `--no-verify`) so this class of packaging bug is caught before the permanent upload. Binary installs (GitHub release, Homebrew, AUR, shell installer) were never affected — they bundle the full workspace. The broken 0.3.0 crates are yanked.

## [0.3.0] — 2026-06-13

### Added
- **Workspace architecture split (A3)** — the monolith is now three library crates (`agent-core`, `agent-engine`, `agent-tui`) behind the `synaps` binary. Cleaner boundaries, faster incremental builds, independent test surfaces. No user-facing CLI changes.
- **Dedicated render thread** — the terminal is driven from its own thread off an immutable `RenderModel` snapshot, decoupling draw latency from the event loop and closing a class of resize/teardown races. The signal watchdog is retired now that rendering is isolated.
- **`/stats` command** — per-session token + cost receipt with a cache-split footer.
- **`/context` command** — inspect live context-window usage.
- **Configurable prompt-cache TTL (`cache_ttl` config key)** — `5m` (default, unchanged behavior), `1h`, or `hybrid` (1h on the stable tools/system prefix, 5m on the per-turn message tail). Long-gap sessions (> 5 min between turns) can keep their cache warm instead of paying full input price every turn. Invalid values fall back to 5m with a boot warning. Spec: `docs/specs/cache-ttl-spec.md` (v4)
  - Pricing is TTL-aware: 1h cache writes billed at 2.0× input (5m stays 1.25×); when the API reports no 5m/1h split, aggregate writes are billed at the 5m rate (fail-cheap)
  - `extended-cache-ttl-2025-04-11` beta header sent only on API-key auth — 1h is live-verified working bare over OAuth, whose beta set is left untouched
  - Silent-downgrade detector: if 1h is configured but the account never honors it, a one-time-per-session notice fires; the configured mode is never auto-flipped
  - RPC protocol: `agent_end.usage` gains optional `cache_creation_5m` / `cache_creation_1h` fields (omitted when unknown — additive, existing consumers unaffected); same split flows through server-mode `Usage` messages and subagent aggregation
- **`/model <name>` and `/thinking` persist to config** — in-session changes survive restarts.
- **Paste while streaming** — input can be pasted mid-turn without dropping characters.
- **Build commit in the TUI header** — the version now shows the exact git commit the binary was built from (e.g. `v0.3.0 · 88e08e0`), with a `-dirty` marker for builds from a modified tree.

### Changed
- **Honest billing** — reported cost now reflects exactly one Usage event per request and TTL-aware cache-write pricing. Figures will differ from 0.2.1 (more accurate, not more expensive).
- **Signal shutdown overhaul** — SIGINT/SIGTERM handling rebuilt around the real root causes: clean teardown ordering, bounded budgets, and no watchdog.

### Fixed
- **The "stopping" bug (#130)** — in large conversations the agent could silently end a turn after running tools, never landing the synthesizing reply. Anthropic delivers some failures (e.g. `overloaded_error`) as an in-stream `error` event under a 200 response; the SSE parser had no `error` arm and dropped it, leaving an empty stream that was treated as a clean end-of-turn. The parser now surfaces these as visible, actionable errors; an empty model response is never swallowed; and the retry budget is unified so HTTP 5xx, transport drops, and in-stream errors all draw from one policy (429 keeps its dedicated budget). Prompt caching is unaffected — retries re-send the byte-identical request, hitting cache.
- **Cache-TTL usage split was read from the wrong SSE event** — live `message_delta` frames carry only the aggregate; the 5m/1h breakdown arrives on `message_start`. The split is now captured at `message_start` with a delta-arm fallback, restoring telemetry split keys, the downgrade detector, and correct 1h write pricing in streaming sessions.
- **Signal delivery + shutdown** — busy-loop and unkillable-process on a dead PTY fixed (three-layer); SIGINT clean-exit path restored.
- **Toast renderer panic** on small-terminal resize (`min > max`).
- **Idle logo gradient** frozen until the first keystroke.
- **Retry backoff** is now header-aware (honours `retry-after`/reset) with a higher 429 budget.
- **`/model`, `/thinking`, `/compact` side effects** now run on the live engine dispatch path.
- **Server-mode clients** now receive `Notice` events.

### Performance
- **Boot:** `--continue` no longer parses every session file (~11s → ~1s); the skills fs-walk moved off the async runtime; `plugins.json` read hoisted out of the extension-load loop.
- **Sessions:** `/sessions` reads headers + recent-only instead of parsing 76MB; `save_chain`'s 76MB collision scan dropped.
- **Runtime:** request body serialized once across retries; `sanitize_thinking_blocks` O(N²) → single tail pass.
- **TUI:** dirty-checked widget events killed the ~30% idle CPU burn (#119); heavy `App` structures Arc-projected; `/extensions` audit bounded to a tail read.

## [0.2.1] — 2026-06-12

### Fixed
- **AUR build failure on LTO-enabled systems** — the rustls migration (0.2.0) introduced `ring`, whose C/asm objects are GCC-LTO-incompatible with `rust-lld`: makepkg's `-flto=auto` produced bitcode the linker can't read, failing every `ring_core_*` symbol. PKGBUILD now sets `options=(!lto)`; stale `openssl` runtime dependency dropped (TLS is rustls since 0.2.0). Binaries unchanged from 0.2.0.

## [0.2.0] — 2026-06-12

### Performance
- **Typed SSE pipeline — per-token DOM allocation eliminated** — the streaming hot loop no longer builds a `serde_json::Value` per event
  - New `runtime/sse.rs::SseLineBuffer` — zero-copy line buffering: lines borrowed as `&str`, memchr (SIMD) newline search, O(1) read-cursor advance with periodic compaction (was two copies + an O(n) drain per line); UTF-8 splits across chunk boundaries handled by construction, with an exhaustive every-offset re-chunking test suite
  - Typed `AnthropicEvent` wire model with `Cow<'a, str>` borrowing — text deltas borrow from the line buffer on the escape-free fast path, own only when JSON escapes force decoding
  - `ParseState` + `process_event` consolidate the three parse sites (main loop, tail-flush, end-of-stream finalize) into a single write path — duplicate-site drift now structurally impossible; 15 behavioral seam tests pin the contract
- **Dirty-flag render loop + streaming redraw coalescing** — the TUI burned a full core during streaming (unconditional full-frame draw on every 16ms tick and every SSE delta); `needs_redraw` flag draws only on actual state change, idle costs zero draws
- **Streaming raised 30→60fps** — redraw coalescing throttle tightened now that frames are cheap
- **Draw-path allocation elimination** — viewport `Paragraph` moves the line Vec instead of deep-cloning every frame; ASCII-art rendering collapses same-style runs from one Span-per-char to one Span-per-line; per-frame version `format!` → `concat!` const
- **`mem::take` at session resume** — `rebuild_display_messages` no longer deep-copies the entire message history (potentially MBs) to dodge the borrow checker
- **Cache strategy: sliding-4 → single-last** — one stationary `cache_control` marker on the last message replaces ~57 lines of sliding-breakpoint logic
  - Bench evidence: equivalent hit rate (96–97% both strategies); live-verified 99% hit on warm turn
  - Prefix-invalidation bug class (mutating old markers) eliminated by construction; 5 new unit tests on a previously untested function

### Fixed
- **SSE double-emit on partial final line** — when the final `content_block_stop` arrived without a trailing newline, the tail-flush pushed the block but left `in_thinking`/`in_tool_use` set, so the end-of-stream flush pushed it again — duplicated content block sent to the model on the next turn
- **PTY zombie reaping** — `PtyHandle::Drop` killed the child but never `wait()`ed; every timed-out or dropped shell session left a zombie. Now reaped with bounded-retry `try_wait()` (all other child-process sites already used `kill_on_drop`)
- **Extension crash-loop health lied** — `restart_count` reset on successful handshake, so an extension that initialized fine and died on the next request never hit the exhaustion limit and reported Running. Consecutive-failure counter now resets only when the restarted process actually serves a call; new `total_restarts` (never reset) feeds health so recovered extensions report Degraded
- **Integration test rot from `env_clear`** — security-hardened extension spawns silently broke 20+ test fixtures parameterized via env vars; fixtures moved to argv parameters, plus poison-cascade and parallel-env-race cleanup — full suite green, 3 consecutive runs
- **Hardcoded Claude Code identity removed from API preamble** — replaced with a configurable `identity` config key + proper SynapsCLI default
- **Dotted (namespaced) config keys exempt from unknown-key warnings**

### Added
- **claude-fable-5 support** — known-models entry with adaptive thinking, pricing ($10/$50 per MTok), and 1M context opt-in
- **Telemetry module** — structured per-request API records: `TelemetryLevel` (off/basic/full), usage with cache TTL breakdown, rate-limit headers, cache-diagnosis records; JSONL writer to `~/.cache/synaps/api-log.jsonl` (0600 + `O_NOFOLLOW`), wired into config keys and the Anthropic SSE stream
- **Cache strategy benchmark suite (`bench/`)** — 21 tool-heavy questions with deterministic outcomes across 4 swappable breakpoint strategies (none / single-last / last-3 / sliding-4); per-turn JSONL logging of tokens, cache read/write, hit %, cost, latency; `compare.py` for side-by-side runs
- **`synaps completions <shell>`** — shell completions via clap_complete (bash, zsh, fish, elvish, powershell)
- **First-run banner** — boot with no OAuth, no `ANTHROPIC_API_KEY`, and no provider keys shows a getting-started banner with the three setup paths instead of a silent TUI

### Changed
- **reqwest 0.11 → 0.12 — duplicate HTTP stack collapsed** — reqwest 0.11 was the sole consumer of hyper 0.14/http 0.2/h2 0.3 alongside axum's hyper 1.x; the old stack (plus native-tls/OpenSSL) is gone, TLS is now rustls with native root certs — explicit in the manifest, smaller binary, zero source changes
- **Humanized API + network errors** — raw JSON dumps and reqwest debug strings replaced with actionable text (529 → "overloaded, wait", 401 → "run synaps login", context overflow → "run /compact"); retry notices ("⏳ API error, retrying…") are now display-only system lines instead of fake assistant content persisted into session history
- **Input recovery on stream error** — when a stream dies after retries, the user's popped message is restored to the input box instead of silently destroyed
- **Config parse warnings with did-you-mean** — unknown keys suggest corrections via levenshtein (`modle` → did you mean `model`?); unparseable values warn and fall back to defaults instead of silently misbehaving
- **Login/status polish** — token-refresh errors now say `synaps login` (was a nonexistent command); `synaps status` refreshes the OAuth token before the request instead of 401ing on expired tokens; `--help` rewritten with real about text and `--profile` documented

### Earlier in this cycle
- **Engine refactor + `chat` headless mode** — `daemon`, `run`, and `client` subcommands deleted; `chat` is now fully-featured headless mode with MCP, extensions, skills, sessions, compaction, and the event bus (same engine as the TUI, stdin/stdout rendering)
  - New `engine/` module: `setup.rs` (boot), `commands.rs` (headless slash commands), `stream.rs` (StreamEvent consumer), `session.rs` (ConversationState)
  - `chatui/` module deleted — `tui/` is the sole frontend
  - New `pricing.rs` — centralized Anthropic pricing (Opus/Sonnet/Haiku) with cache billing support
  - New `harbor/synaps_agent.py` — Terminal-Bench integration agent
  - Clippy CI added (`cargo clippy --all-targets` gates PRs)
  - Published on crates.io as `synaps` (v0.1.x); available via `cargo install synaps`, AUR (`yay -S synaps`), Homebrew, and GitHub Releases
- **Path B Phase 6 — The Big Move (extension contracts)** — synaps-cli core no longer contains whisper-specific code; the local-voice plugin owns it
  - Deleted `src/voice/{models,download,rebuild}.rs` (whisper.cpp catalog, HuggingFace downloader, cmake-rebuild orchestrator)
  - Slimmed `src/voice/discovery.rs` from ~400 LOC to ~190: only `discover()` / `discover_in()` / `DiscoveredVoiceSidecar` remain; `read_build_info`, `detect_host_backend`, `probe_*` are gone (callers use the Phase 5 `info.get` cache instead)
  - Deleted in-core `/voice models|help|download|rebuild` action arms and parsers — those subcommands now route through the plugin via the Phase 2 interactive command contract; missing-plugin case errors with a clear message
  - Deleted in-core whisper download UI: `App.{download_progress, download_filename, download_rx, voice_download_in_flight, model_browser_selected, cached_voice_compiled_backend}` plus `start_download` / `on_download_progress` / `on_download_complete` and the `download_change` event-loop arm; the sticky progress widget now exclusively renders generic `extensions::active_tasks::ActiveTasks` (Phase 3)
  - Deleted `EditorKind::{ModelBrowser, WhisperModelPicker}`, `ActiveEditor::ModelBrowser`, `ModelBrowserRow`, `model_browser_rows`, `whisper_model_options`, `render_model_browser`, `render_download_progress_line`, plus the `voice_stt_model` / `voice_stt_backend` / `voice_language` setting definitions — the plugin replaces them via the Phase 4 settings categories contract
  - **Migration shim:** on first launch, legacy global voice config keys (`voice_stt_model_path`, `voice_stt_model`, `voice_stt_backend`, `voice_language`) are copied into the plugin namespace `~/.synaps-cli/plugins/local-voice/config` if the destination is empty; legacy keys remain in place for safety. Reads at runtime prefer the plugin namespace and fall back to the legacy global key with a deprecation warning.
  - Net change: `~−2,400` LOC removed from synaps-cli core; behaviour preserved when the local-voice plugin is installed
- **Phase 2 — Extensions Capability Platform** — extensions are now first-class citizens
  - Slash command extension contract (`commands` in plugin manifest)
  - Settings panel extension contract (`settings` in plugin manifest)
  - Keybind extension contract (User-tier binds via plugin manifest)
  - Model-provider extension contract — extensions can register OpenAI-compatible providers as full agentic backends with tool-call support
  - Capability discovery + permission gating per extension
  - Slices P–W complete; 836 tests green (682 lib + 154 bin)
- **Voice dictation** — toggle-driven mic capture wired to the input buffer
  - `/voice` slash command with subcommands: `models`, `download <id>`, `help`, `rebuild [backend]`
  - Configurable toggle key — defaults to F8; supported: F8, F2, F12, C-V, C-G (Ctrl+Shift and Ctrl+Alt are unreliable in terminals)
  - Toggle-only flow: press once → start listening (🎤 listening pill), press again → stop, transcribe, append to input
  - VAD-aware: final transcripts during an armed session insert text without disarming
  - Settings → Voice category: toggle key, language cycler (14 langs), STT model, STT backend
  - `voice_language` cycler: `auto / en / es / fr / de / it / pt / nl / ja / zh / ko / ar / hi / ru`
  - Hot-reload keybind on settings save — no restart required
  - Kitty keyboard protocol enabled for reliable F-key + modifier capture
  - Voice sidecar lives in the `local-voice-plugin` (synaps-skills repo) — synaps core stays voice-free
- **Whisper model manager** — discover, download, switch models in-app
  - 10-entry catalog hard-coded in `src/voice/models.rs`: tiny / base / small / medium / large-v3 / large-v3-turbo + `.en` variants, with real SHA256s from HuggingFace
  - `/voice models` renders an installed/uninstalled table
  - `/voice download <id>` — streaming download with atomic `.partial → rename` install, SHA256 verification, cancellable, single-in-flight
  - `EditorKind::ModelBrowser` — Settings → Voice → STT model browses the catalog inline, Up/Down to nav, Enter installs-or-selects
  - Inline footer download progress while a model fetches
  - Models live under `~/.synaps-cli/models/whisper/`
- **Whisper backend selection** — pick CPU / CUDA / Metal / Vulkan / OpenBLAS or auto-detect
  - `voice_stt_backend` cycler in Settings → Voice
  - `auto` probes the host (`nvcc`, `vulkaninfo`, `pkg-config`, target-os) and picks the best available
  - "Current build: <backend>" annotation surfaces the sidecar's compiled feature set
  - ⚠ rebuild annotation when the selected backend differs from the compiled one
  - `/voice rebuild [backend]` invokes `local-voice-plugin/scripts/setup.sh --features <backend>` and streams build output as System rows
  - Sidecar reports its build via `--print-build-info` (JSON: `{backend, features, version}`)
- **Chat UI rendering polish**
  - System / Error messages now split on `\n` and word-wrap on each sub-line (same path as User / Text)
  - Tab-aware soft wrap: continuation rows indent under the last-tab anchor column (4-col tab stops); safety clamp falls back to leading-space indent if the anchor exceeds 60% of width
  - Fixes `/voice help`, `/chain ls`, `/keybinds`, plugin command output rendering
- **Extension System** — process-based JSON-RPC hooks (before_message, before_tool_call, after_tool_call, on_session_start, on_session_end)
  - Tool-specific filtering: hooks can target specific tools
  - Context injection via HookResult::Inject
  - Permission-gated with 6 permission types
  - Drop-in plugin support
  - Silent loading (only show failures)
- **Watcher notify_inbox hook** — event bus notification on agent completion
  - Config option: `[hooks] notify_inbox = true` in watcher agent config
  - Drops JSON events into `~/.synaps-cli/inbox/` when agents complete
  - Works in both 'once' and 'deploy' modes
  - Event payload includes agent name, session number, elapsed time, exit code, timestamp
- **Table rendering improvements**
  - Cells wrap instead of truncating with ellipsis
  - Inline markdown (bold/italic/code) rendered in table cells
  - Smarter column shrinking — preserves narrow columns
- **Structural refactoring** — improved code organization
  - `catalog.rs` → `catalog/` directory with per-provider modules
  - `palettes.rs` → `palettes/` directory with per-palette files
  - `tools/subagent*.rs` → `tools/subagent/` directory
  - `tools/tests.rs` → distributed inline test modules per tool
  - `runtime/api.rs` split into `api.rs` + `api_sync.rs` + `request.rs`
  - `chatui/mod.rs` helpers extracted to `helpers.rs` + `lifecycle.rs`
- **UTF-8 char boundary panics** — multiple fixes for multi-byte character handling
  - Hook output truncation now finds valid char boundaries
  - Bash output highlighter fixed for emoji/CJK characters
  - Additional edge cases found by PR review
- **Hook system improvements**
  - Handle HookResult::Block in before_message hook
  - Set tool_runtime_name in all before_tool_call hook paths
  - Track truncation explicitly in bash tool instead of string matching

### Added
- **Multi-Provider Runtime** — OpenAI-compatible provider engine
  - 17 providers: Groq, Cerebras, NVIDIA NIM, OpenRouter, Google AI Studio, DeepInfra, HuggingFace, Fireworks, Hyperbolic, Scaleway, SiliconFlow, Together, Chutes, Codestral, Perplexity, OVHcloud, + Local (Ollama/LM Studio/vLLM)
  - 55+ models cataloged with tier ratings (S+/S/A+/A/B)
  - `provider/model` shorthand: `/model groq/llama-3.3-70b-versatile`
  - SSE streaming with tool calling across all providers
  - StreamDecoder with HashMap-based tool call accumulation
  - Anthropic↔OpenAI message/tool translation layer
  - Provider router in `api.rs` — model prefix routes automatically
  - Config: `provider.<name> = <key>` in `~/.synaps-cli/config`
  - Env var fallback: `GROQ_API_KEY`, `CEREBRAS_API_KEY`, etc.
  - Local model support: `provider.local.url = http://localhost:11434/v1`
- **`/ping` command** — health-check all configured provider models
  - Non-blocking, results stream in live as each model responds
  - Shows ✅/❌/⏳/🔒/⌛ with latency per model
  - Results cached for model picker display
- **Settings → Providers** — TUI panel for API key management
  - View all 17 providers with key status (✅ set / ⬚ not set)
  - Enter to set/update key, d/Del to clear
  - Key masking (last 4 chars only)
  - Local endpoint URL editing
  - Press `p` to ping all models
  - Scrollable list with overflow indicators
- **Settings → Model Picker** — expanded to show provider models
  - Models grouped by provider with headers (── Groq ──, etc.)
  - Only shows providers with keys configured
  - Header rows are unselectable (skip on navigation)
  - Health status shown when ping data available
- **App starts without Anthropic credentials** — users with only provider keys can boot the TUI and use free models
- **Session Naming** — `/saveas <name>` aliases for sessions
  - Name format `[a-z0-9-]{1,40}`, validated and collision-checked
  - `/saveas` (no arg) clears the name
  - `synaps --continue <name>` resolves session names
  - `/sessions` shows `[@name]` tags on named sessions
- **Chain Naming** — `/chain name/list/unname` bookmarks for compaction lineages with auto-advance
  - `/chain name <name>` bookmarks the current session's lineage
  - `/chain list` shows all named chains (`*` marks active)
  - `/chain unname <name>` removes a bookmark
  - `/chain` (no args) shows lineage + "bookmarked by: @name" if present
  - Chain pointers stored at `~/.synaps-cli/chains/<name>.json`
  - On `/compact`, chain pointers auto-advance to the new session
- **Unified Session Resolution** — `resolve_session()`: chain name → session name → partial ID, used by `--continue` and `/resume`
  - Shared by `synaps --continue`, `/resume`, and server `--continue`
  - Resolution path surfaced as a system message (`↳ resolved via chain 'foo'` / `↳ resolved via name 'bar'`)
  - `--continue` value_name changed from `SESSION_ID` to `NAME_OR_ID`
- **Event Bus** — universal message ingestion for agent sessions
  - `synaps send` CLI command with atomic file writes
  - inotify inbox watcher via `notify` crate (spawn_blocking, non-blocking)
  - Priority EventQueue with severity ordering (Critical→front, High→after)
  - `tokio::sync::Notify` for instant TUI wake on event push
  - Events auto-trigger model turns when agent is idle
  - Events buffer during streaming via `pending_events`, flush on completion
  - Styled TUI event cards with severity icons (🔴🟠🟡🔵)
  - XML-wrapped event format with prompt injection hardening
  - 256KB file size cap, symlink guard, 0700 permissions
- **Reactive Subagents** — dispatch, poll, steer, collect
  - `subagent_start` — spawn and return handle_id immediately
  - `subagent_status` — non-blocking progress snapshot
  - `subagent_steer` — inject guidance mid-flight via steering channel
  - `subagent_collect` — non-blocking result check
  - `subagent_resume` — restart timed-out agents (stub)
  - `SubagentHandle` with shared `RwLock<SubagentState>`
  - `SubagentRegistry` with cleanup_finished on stream Done
  - Abort cancels all running reactive subagents
  - Thread handles stored for graceful shutdown
  - 13 unit tests for handle + registry
- **Chain Sessions** — `/compact` creates linked child session
  - Old session preserved on disk with `compacted_into` forward link
  - New session starts with `parent_session` back link
  - `/chain` command walks the session lineage
  - System prompt included in compaction summary
  - Configurable compaction model (defaults to Sonnet)
  - ModelPicker for compaction model in settings
  - Non-blocking compaction with spinner animation
  - Compacted summary hidden from TUI display
  - Message queuing during compaction
- **`respond` tool** (stub — returns honest failure until wired)
- **`send_channel` tool** (stub — returns honest failure until wired)
- **`/status` command + `synaps status` subcommand**: check account usage (5-hour, 7-day, Sonnet) with progress bars and reset countdowns. Hits OAuth usage API.
- **`/compact` slash command**: summarize & compact conversation history when context gets long
  - Structured checkpoint format (goals, progress, decisions, file ops, next steps)
  - Iterative compaction — re-compacting merges new work into existing summary
  - File operation tracking (read/write/edit paths preserved across compactions)
  - Custom focus instructions via `/compact <focus>`
  - Uses dedicated low-effort API call (no tools, summarization system prompt)
- **`context_window` setting wired to API**: `200k` (default) omits beta header; `1m` sends `context-1m-2025-08-07` on supported models (Opus 4.6+, Sonnet 4.x); previously was UI-only display cap
- **Claude Code marketplace compatibility**: probe both `.synaps-plugin` and `.claude-plugin` layouts, `${CLAUDE_PLUGIN_ROOT}` substitution in skill bodies
- **Plugins subdir sources and cascade uninstall**: install from subdir-based plugin repos, cascade-remove plugins when their marketplace is deleted
- **Settings → Plugins marketplace overlay**: "Open Plugin Marketplace" action row in Settings, opens plugins modal as nested overlay
- **Hidden binary**: GamblersDen bundled as `hidden` binary alongside `synaps`
- **Single binary architecture**: all binaries consolidated into `synaps`
  - `synaps` (no args) = TUI (was `chatui`)
  - `synaps chat` = fully-featured headless mode (MCP, extensions, skills, sessions)
  - `synaps server` = WebSocket API (was `server`)
  - `synaps agent` = headless worker (was `synaps-agent`)
  - `synaps watcher` = supervisor (was `watcher`)
  - `synaps login` = OAuth (was `login`)
  - `synaps send` = push events into a running session
  - `synaps status` = check account usage
  - (Removed: `run`, `client`, `daemon`)
- **Live theme preview in /settings**: scroll themes to preview, Enter confirms, Esc reverts
- **Theme hot-reload**: `/theme <name>` applies instantly without restart (ArcSwap)
- **Settings picker scroll**: theme/model picker scrolls with cursor
- **Adaptive thinking for Opus 4.7+**: `{type: "adaptive", display: "summarized"}` with effort mapping (xhigh/high/medium/low/adaptive)
- **Model-agnostic context window**: bar denominator adapts per-model (1M Opus 4.7, 200K Sonnet/Haiku)
- **Per-turn context tracking**: usage bar shows actual request size, not cumulative cost
- **Effort parameter**: thinking depth on adaptive models controlled via `output_config.effort`
- **"adaptive" thinking option**: new cycler value in `/settings` — lets model decide thinking depth
- **Tab-cycle for slash commands**: `/s` + Tab cycles through sessions → settings → system
- **Streaming command guard**: known slash commands no longer leak into model stream as steering
- **Usage log opt-in**: `SYNAPS_USAGE_LOG=1` writes to `~/.cache/synaps/usage.log` (0600, O_NOFOLLOW)
- **Opus 4.6 in model picker**: was missing from `/settings` model list
- **`settings` + `plugins` in tab-complete**: were missing from `BUILTIN_COMMANDS`
- **Plugin Keybinds** — plugins declare custom keyboard shortcuts in `plugin.json`
  - `KeybindRegistry` with parser, matching, and conflict resolution
  - Key notation: `C-x` (Ctrl), `A-x` (Alt), `S-x` (Shift), `F1`–`F12`, special keys
  - 4 action types: `slash_command`, `load_skill`, `inject_prompt`, `run_script`
  - User overrides via `keybind.*` in config — override or disable plugin binds
  - Priority: core > user > plugin (core binds never overridable)
  - `/keybinds` command shows all registered binds with source attribution
  - 23 unit tests for parser, registry, conflicts, overrides
- **Plugin Agent Namespaced Resolution** — `subagent(agent: "dev-tools:sage")` resolves plugin agents via `plugin:agent` syntax
  - Searches `plugins/<plugin>/skills/*/agents/<agent>.md`
  - Input validation, path traversal protection, ambiguity detection
  - Updated error messages to mention `plugin:agent` syntax
- **Mouse Text Selection & Clipboard** — left-click drag to select text in chat area
  - Right-click with selection → copy to system clipboard
  - Right-click without selection → paste from clipboard
  - Singleton clipboard thread (no thread-per-copy accumulation)
  - Paste suppression with 150ms TTL (prevents terminal double-paste)
  - Selection highlight via `Block::inner()` computed content rect
  - Theme-aware highlight color

### Fixed
- **Empty thinking/text blocks rejected by API** — fixed two related 400 errors that could permanently brick a session:
  - `messages.N.content.M.thinking: each thinking block must contain thinking` — streaming accumulator (`runtime/api.rs`) was pushing thinking content blocks even when no `thinking_delta` arrived (only a signature, or stream cut off). Now skipped at all three accumulation sites (mid-stream, tail buffer, post-loop fallthrough).
  - `messages: text content blocks must be non-empty` — defensive sanitizer (`runtime/helpers.rs::sanitize_thinking_blocks`) added; runs on every outbound API call from both `call_api_stream_inner` and `call_api`. Strips empty `thinking`, `redacted_thinking.data`, and `text` blocks; drops assistant messages whose entire content gets stripped; merges resulting consecutive same-role messages to preserve Anthropic's strict role-alternation rule. Auto-heals sessions persisted before the streaming fix landed.
  - 9 unit tests covering the drop-and-merge cases.
- **Config file permissions** — now 0600 (was 0644, world-readable with API keys)
- **API key masking** — shows last 4 chars only (was showing first 8 + last 4)
- **ProviderConfig Debug** — custom impl redacts `api_key` as `[REDACTED]` in logs
- **Provider base URLs** — corrected siliconflow (.com→.cn), chutes (→llm.chutes.ai)
- **Dead models removed** — groq/llama-3.1-70b, groq/qwen-qwq-32b, groq/gemma2-9b (all decommissioned)
- **Google stream_options** — skip `include_usage` for googleapis.com (rejects it)
- **Tool→user message ordering** — insert space-content assistant between tool result and user message for strict providers
- **Finish-frame tool call dedup** — NVIDIA sends final argument chunk with finish_reason; was being dropped as "resend"
- **Model ID extraction** — health prefix (✅ 253ms) no longer embedded in model string sent to API
- **`/status` on non-Anthropic** — shows friendly message instead of crashing
- **"Calling Claude..."** — now shows actual model name in `synaps run`
- **Paste in settings** — `Event::Paste` now handled in API key, text, and custom model editors
- **Missing provider key** — `/model sambanova/llama` with no key shows clear error instead of silent Anthropic misroute
- **UTF-8 truncation panic** — `bash` and `shell` output truncation now finds valid char boundaries before truncating, preventing crash on multi-byte characters (emoji, CJK)
- **Zero-width terminal panic** — markdown word-wrap `chunks(0)` crash on rapid tmux pane resize, clamped to `.max(1)`
- **Local model connection** — friendly "is Ollama running?" instead of raw TCP error
- **`/help` updated** — mentions provider/model syntax and /settings for key management
- **`ping_print` lifecycle** — resets after all results arrive via pending counter
- **MCP tool name causes 400 errors** — Anthropic rejects `mcp_` prefixed tool names (rate limit pool misrouting). Renamed `mcp_connect` → `connect_mcp_server`, tool prefix `mcp__server__tool` → `ext__server__tool`.
- **`/saveas` on empty sessions** — `save_session()` bailed on empty `api_messages`, so the name never persisted. Now calls `session.save()` directly.
- Compaction no longer overwrites original session (loads from disk)
- Event content sanitized against prompt injection (XML tags, case-insensitive)
- Atomic inbox writes (.json.tmp → .json rename)
- inotify watcher runs on spawn_blocking (no longer starves tokio runtime)
- Events during streaming buffered and flushed after MessageHistory
- Spurious auto-triggers prevented by event_received guard
- push() calls notify_one() (was missing — events silently queued)
- Session save ordering: new session saved before old session updated
- chars().count() moved outside lock scope
- High-priority event FIFO ordering (was LIFO among Highs)
- Queue-full events left in inbox for retry (not silently dropped)
- push_priority logs evicted event ID
- **Session file corruption**: atomic writes via write-to-tmp then rename
- **Tool input parse errors surfaced to model**: malformed tool_use JSON no longer silently falls through to empty input; model sees `invalid tool input JSON: ...` and can self-correct
- **Custom theme crash**: `unreachable!()` in draw replaced with graceful fallback colors for non-Rgb themes
- ASCII logo alignment: use unicode display width for consistent centering
- 'default' theme missing from settings picker
- Context bar pinned at 100% after 2-3 turns (was using cumulative tokens / hardcoded 200K)
- Thinking blocks invisible on Opus 4.7 (display defaulted to "omitted")
- `budget_tokens: 0` sentinel leaked to non-adaptive models → 400 error
- `/settings` cycling capped at xhigh (`apply_setting` silently rejected "adaptive")
- Stale test asserting `thinking_level_for_budget(0) == "low"` (production returns "adaptive")
- Usage log world-readable at `/tmp/` → moved to `~/.cache/` with 0600

### Changed
- Compaction logic moved from chatui to `core/compaction.rs`
- `SubagentHandle`/`Registry`/`Status` moved to `runtime/subagent.rs`
- `ApiOptions` struct replaces `use_1m_context: bool` threading
- `build_auth_header` + `build_beta_header` extracted as helpers
- `clone_repo` helper deduplicates plugin installer
- All compaction prompts colocated in `core/compaction.rs`
- Tool descriptions guide model choice (subagent vs subagent_start)
- Stub tools unregistered from tool registry (model doesn't see unimplemented tools)
- SubagentState uses RwLock instead of Mutex
- **`define_settings!` macro**: settings schema + apply handler defined once in `settings/defs.rs` via declarative macro — zero drift possible (replaced manual sync + parity tests)
- **Single source of truth for commands**: removed duplicate `ALL_COMMANDS` array; `commands.rs` now sources from `skills::BUILTIN_COMMANDS`
- **`src/cmd_*.rs` → `src/cmd/` module**: subcommand handlers moved to dedicated directory, `cmd_` prefix stripped
- Binary name: `chatui`/`synaps-cli`/`synaps-agent` → `synaps` (single binary with subcommands)
- `src/chatui/main.rs` → `src/chatui/mod.rs` (module, not binary)
- watcher spawns `synaps agent` instead of standalone `synaps-agent`
- `thinking_level_for_budget()` consolidated from 4 copies into single source of truth in `core/models.rs`
- `DEFAULT_LEGACY_ADAPTIVE_FALLBACK` constant replaces magic `16384` in clamp sites
- Dead-code warnings suppressed with explanatory comments (reaper handles, settings help field)
- Auto-cache toggle removed (manual breakpoints won: 90% vs 53%)
- README rewritten from internal documentation to product landing page

### Removed
- `SPEC-WATCHER.md` — internal spec, not needed in repo
- Auto-cache config toggle (`auto_cache = true/false`)

## [0.1.0] — 2026-04-12

### Added
- Interactive TUI (`chatui`) with streaming, markdown, syntax highlighting
- Headless chat (`chat`) for scripting and piping
- Autonomous agent supervision (`watcher`) with heartbeat, cost limits, handoff
- 10 built-in tools: bash, read, write, edit, grep, find, ls, subagent, mcp_connect, load_skill
- Interactive shell sessions: shell_start, shell_send, shell_end
- 18 color themes
- MCP integration with lazy server spawning
- Skills & plugins subsystem
- OAuth + API key authentication
- WebSocket server/client transport
- `/settings` full-screen modal
- `/plugins` management UI
- Session persistence and `/resume`
