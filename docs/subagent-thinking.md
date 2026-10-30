# Subagent thinking

`subagent`, `subagent_start` and `subagent_resume` accept an optional `thinking`
argument: `low`, `medium`, `high`, `xhigh`, or `max`.

New workers use `subagent_thinking = xhigh` by default. This config key is
independent of the foreground `thinking` key. A call override wins. The value
is resolved once when the call is admitted, then applied to the worker runtime
**after model selection and inherited execution-mode setup**, before its first
request. Unsupported model/effort combinations are refused rather than silently
lowered. Provider capability validation remains authoritative.

Reactive workers record the selected effort in their existing resume archive.
Resume preserves that value even if the global config changes, unless the resume
call supplies `thinking`. Archives from older versions, which do not record an
effort, use the current subagent default. The resumed run records the resulting
value for subsequent resumes. Running workers are not mutated by config edits.

Examples:

```json
{"task":"Review this change", "system_prompt":"Review only", "thinking":"high"}
```

```json
{"handle_id":"sa_3", "instructions":"Continue", "thinking":"xhigh"}
```

The `/settings` Subagent thinking row changes the default for later calls. It
does not change foreground thinking or already-running workers.

## Astra wire behavior

The Codex route uses its typed execution plan, not the generic
`openai_effort_for_level` helper. Its exact Astra capability table and recorded
catalog fixture support `xhigh` and `max`; the Codex body serializes these values
unchanged. Tests cover the resolved worker effort through runtime setup into
that production request-body builder. A live catalog row, when present, remains
authoritative. This is fixture/source evidence, not a new authenticated provider
probe. The generic OpenRouter/Groq mapping is unchanged.
