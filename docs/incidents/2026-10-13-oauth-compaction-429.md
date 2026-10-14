# Incident 2026-10-13: OAuth compaction fails with HTTP 429

**Status:** fix proposed in `fix/compaction-oauth-transport-identity`.
**Affected:** remote compaction (`/compact`, auto-compaction) on Anthropic
OAuth logins whose config does not override `identity`. Every release through
0.10.2. Code references are to `dev` at `ae490b32`.

## Symptom

Compaction fails after several minutes with:

```
[compaction failed: Tool execution failed: Rate limited by Anthropic
 (HTTP 429 [rate_limit_error]). Wait for the limit to reset, or switch models with /model.]
```

The log shows the full retry ladder for each attempt:

```
WARN Compaction API retry after 1s: 429 Too Many Requests: HTTP 429
WARN Compaction API retry after 2s: 429 Too Many Requests: HTTP 429
...
WARN Compaction API retry after 128s: 429 Too Many Requests: HTTP 429
```

Ordinary turns on the same token, in the same minutes, keep succeeding. That
makes it look like a quota problem. It is not.

## Evidence

Two benchmark runs on 2026-10-13 used a clean `HOME` (default config, real
OAuth token):

| Run | Compaction model | Main turns | Compaction requests |
|---|---|---|---|
| `long` | `anthropic/claude-sonnet-4-6` | 200 OK | full 429 ladders at 23:03 and 23:08 UTC (one earlier attempt failed with an auth error) |
| `compact-opus` | `anthropic/claude-opus-5-5` | 50 rounds, all 200 OK | 429 on all 17 requests, across ladders at 23:46 and 23:52 UTC |

Request traces for a main round and the compaction call right after it differ
in the system prefix. The main round has three synthesized system segments,
the compaction call has two, and its first segment is different.

Live A/B, same OAuth token, same minute, `claude-sonnet-4-6`:

| System blocks | Result |
|---|---|
| `[default identity, guidance, prompt]` (what compaction sends) | 429 `rate_limit_error` |
| `[Claude Code transport identity, default identity, guidance, prompt]` (what turns send) | 200 |

## Root cause

Anthropic routes OAuth requests on the first system block, which must be the
Claude Code transport identity:

```
You are Claude Code, Anthropic's official CLI for Claude.
```

Without it, the request is refused with a 429 `rate_limit_error`. That is
misleading, but it is what the API returns.

`4ada8cbd` ("preserve Anthropic OAuth transport identity", 2026-07-16) made that
string the fixed first block of the streaming path,
`HelperMethods::build_system_blocks` in `runtime/helpers.rs`. Compaction does
not use that builder. It assembles its own blocks in
`compaction_system_blocks` (`runtime/api_sync.rs`), and that function still
led with `get_identity()`, the configurable product identity. The default is:

```
You are an AI assistant running in SynapsCLI, an open-source agent runtime.
```

Configs that set `identity = You are Claude Code, Anthropic's official CLI for
Claude.` happen to put the right string first, which hid the bug on machines
where it was set (including the maintainer's).

A second, cosmetic bug showed up in the same logs: the pre-dispatch disclosure
printed a provider-qualified compaction model twice, as
`anthropic/anthropic/claude-opus-5-5`.

## Fix

- Hoist the transport identity and the general guidance to shared constants in
  `helpers.rs` and use them in both builders. Compaction's OAuth system prefix
  is now text-identical to the streaming path (still without cache markers,
  since compaction is a one-off request).
- The translation report records three synthesized segments for the OAuth
  compaction prefix, matching what is actually sent.
- The API-key path is unchanged byte for byte.
- The disclosure line names the provider once.

Tests: compaction OAuth blocks lead with the transport identity and match the
streaming prefix; the report has exactly three synthesized entries; the
API-key path is unchanged; the disclosure names the provider once.

## Workaround (before the fix ships)

Set the identity in `~/.synaps-cli/config`:

```
identity = You are Claude Code, Anthropic's official CLI for Claude.
```

Or use local compaction (`compaction_mode = local`).

## Follow-ups

- Two code paths built the same wire prefix independently, and only one was
  under a golden-body test. Any new Anthropic request path should build its
  system blocks through `build_system_blocks`, or share its constants, so the
  transport contract has one source.
- On OAuth, a compaction 429 while main turns succeed is a signal worth
  surfacing directly ("compaction refused; main turns OK") instead of spending
  about four minutes in the retry ladder per attempt.
- `runtime::helpers::tests::system_blocks_oauth_transport_identity_first_and_marker_on_last`
  asserted that the configured identity differs from the transport line. The
  identity is a process-wide `OnceLock`, so the test failed whenever a sibling
  test had loaded a config with the override set. Overriding is valid config,
  so the fix PR drops that assert. The underlying problem remains: some tests
  read the real `~/.synaps-cli/config` instead of a hermetic one.
