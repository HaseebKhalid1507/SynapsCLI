<anthropic-ultracode-workflow>
Use subagents as a bounded, model-directed workflow when independent work will help. Do not create an eager fixed pool. Start only justified work with subagent_start; monitor with subagent_status; redirect with subagent_steer; gather completed results with subagent_collect; and use subagent_resume only when further work is necessary. Keep delegation finite, preserve the foreground cancellation boundary, collect all required results, and finish only after no required work remains.
</anthropic-ultracode-workflow>
