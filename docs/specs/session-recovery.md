# Long-session delegation and interrupted stream recovery

## Evidence / scope

- Default host installs a manifestless `8 concurrent / 64 total` delegation policy. `WorkerRegistry::total` never decreases on reconciliation; this session reproduced `dispatch_denied code=total_limit` with no network attempt. Model changes cannot repair it legitimately. Explicit manifest policies must keep their cumulative cap.
- Codex, xAI Responses and generic Chat Completions body-stream errors return directly despite retry support for request dispatch / selected terminal failures. Anthropic already retries body errors. Provider previews are emitted before the final response; tools execute only after a successful complete response in the agent loop.

## Contract

1. Manifestless baseline's total limit bounds outstanding (not reconciled) workers. All terminal states still occupy that limit until collect+reconcile. Concurrency, tree bounds, model trust, write scopes and completion gates remain enforced. Explicit manifest/constructed policies retain cumulative limits. Use a separate monotonic identity counter: rollback must never recycle/collide worker identities. Never reset the whole orchestration runtime to reclaim capacity.
2. Retry interrupted built-in HTTP response streams (including EOF without a terminal frame) with bounded backoff. Reuse the identical request bytes; no tool execution or history insertion from unsuccessful attempts. Honor cancellation before dispatch/during reads/backoff, including outer deadline cancellation. No account/model switch in the transport.
3. Add provider response-attempt boundary/reset events so live previews can be discarded before replay. Previous successful rounds, executed tool results, human steering and usage survive. Plain terminal renderers cannot erase scrollback: mark the discarded attempt visibly; structured consumers receive reset semantics. Worker accumulation/auto feedback must not treat discarded output as final work.
4. Residual reported usage from failed attempts remains chargeable; never fabricate missing usage. Successful wire terminal followed by a connection error is success, not a replay of the completed response. Provider-declared permanent failure/refusal is not a transport retry.
5. Tests offline with local mock HTTP/broker streams: >64 reconciled workers; terminal unreconciled and concurrency limits; explicit cumulative policy; rollback ID uniqueness; partial text/tool EOF→success with identical payloads and one accepted tool; bounded exhaustion, cancellation during backoff, clean truncated EOF, terminal-then-disconnect; frontend reset handling.

No installs, provider inference, private configuration edits, memory migration or service restarts. Preserve existing dirty work. Cargo jobs <=8, verification serialized (environment/PTY tests use one thread).
