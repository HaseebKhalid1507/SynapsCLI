//! SubagentResumeTool — continue a finished subagent with new instructions.
//!
//! The worker's own conversation (from its resume archive, see `archive.rs`) is
//! replayed as the new run's history, with the instructions as the next user
//! turn, so it continues where it stopped rather than starting over. That works
//! for any terminal status and after the handle was collected or reaped; an
//! `archive_path` resumes a worker of an earlier or crashed session. Without an
//! archive (legacy), the prior run's text is pasted into a fresh task. The caller
//! gets a new `handle_id` for the continuation run.

use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, oneshot};

use super::super::{Tool, ToolContext, NEXT_SUBAGENT_ID};
use super::archive::{CompactedResume, ResumePolicy};
use super::lineage::ResumeRetrieval;
use super::record::WriterState;
use super::record_file::{self, RecordLocation};
use crate::memory_backend::MemoryBinding;
use crate::runtime::subagent::{SubagentHandle, SubagentResult, SubagentState, SubagentStatus};
use crate::{AgentEvent, Result, RuntimeError};

pub struct SubagentResumeTool;

fn expired_context_error(handle_id: &str) -> RuntimeError {
    RuntimeError::Tool(format!(
        "Subagent '{}' has expired resumable context; call subagent_collect with reconciled=true to reconcile its retained tombstone.",
        handle_id
    ))
}

/// Identity and opening messages of a resumed run, admitted before any
/// handle, reservation or runtime exists.
#[derive(Debug)]
struct PriorRun {
    agent_name: String,
    model: String,
    system_prompt: String,
    timeout_secs: u64,
    /// Directory the prior run worked in, when recorded (a live handle's or
    /// a v1 archive's; a v2 record has none).
    cwd: Option<std::path::PathBuf>,
    /// Worker effort recorded for the prior run; None in older archives.
    thinking: Option<String>,
    messages: Vec<crate::SharedMessage>,
    /// Set when a cold, large history was replaced by a compact context.
    compacted: Option<CompactedResume>,
    /// A watch worker's lease used so far; `None` for an ordinary worker.
    watch_usage: Option<crate::runtime::subagent::WatchUsage>,
    /// How the resumed worker may see its earlier context.
    retrieval: ResumeRetrieval,
    /// For the caller, e.g. a resume from a crash checkpoint.
    warning: Option<&'static str>,
    /// The lineage a rollover-enabled successor continues: a v2
    /// predecessor's; `None` starts a fresh one (v1 archive, history kept
    /// in memory, or the prior run's text).
    lineage: Option<super::lineage::SuccessorLineage>,
}

/// Where resumes look for records, and the binding a resumed worker runs on.
pub(crate) struct ResumeEnv<'a> {
    /// The trusted archive root (`<base>/subagent-history`).
    pub root: &'a std::path::Path,
    /// This process's directory name below the root.
    pub own_process_dir: &'a str,
    /// The memory binding the resumed worker will run on, from the spawn's
    /// config snapshot and the prior run's directory (when it had one that
    /// still exists; else the resuming session's); asked for only when a
    /// current (v2) record is admitted, and for a rollover run's plan.
    pub binding: &'a (dyn Fn(&crate::config::SynapsConfig, Option<&std::path::Path>) -> MemoryBinding
                 + Send
                 + Sync),
    /// The registry a resumed worker starts from; `None`: this process's
    /// worker registry ([`super::worker_tools`]).
    pub worker_tools: Option<&'a crate::ToolRegistry>,
    /// Test scaffolding (S0), never set in production: the resumed run
    /// gets a plan at this site whatever it wants, as a oneshot's test site
    /// gives one. Only its root and process directory are used.
    #[cfg(test)]
    pub(super) planned: Option<super::oneshot::RecordSite>,
}

/// A live handle's identity: agent, model, system prompt, timeout, (a
/// watch worker) its lease used so far, its effort and its directory.
type Identity = (
    String,
    String,
    String,
    u64,
    Option<crate::runtime::subagent::WatchUsage>,
    Option<String>,
    Option<std::path::PathBuf>,
);

/// The spawn's one config snapshot and the cold-resume policy it sets.
#[derive(Clone, Copy)]
struct ResumeSnapshot<'a> {
    config: &'a crate::config::SynapsConfig,
    policy: ResumePolicy,
}

/// Where the prior context comes from, found under the registry lock.
enum PriorSource {
    /// A finished worker's history kept in memory: its archive could not be
    /// written. `cache_age`: its recorded anchor's, else time since it
    /// finished (no archive file to date it by).
    Memory {
        history: Vec<Value>,
        cache_age: Option<std::time::Duration>,
    },
    /// A worker record file of this or an earlier process.
    File {
        location: RecordLocation,
        writer: WriterState,
    },
    /// Legacy: no archived conversation, only the prior run's text.
    Text(String),
}

fn file_error(prior_handle_id: &str, error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Tool(format!(
        "Cannot resume '{prior_handle_id}': {error}. Nothing was started."
    ))
}

/// The prior run's live identity (if its handle is still known) and where its
/// context comes from. Holds the registry lock; performs no admission.
fn find_prior(
    registry: &std::sync::Mutex<crate::runtime::subagent::SubagentRegistry>,
    prior_handle_id: &str,
    explicit: Option<RecordLocation>,
    env: &ResumeEnv<'_>,
) -> Result<(Option<Identity>, PriorSource)> {
    // An explicit path's run-lock probe is filesystem work, so it runs before
    // the registry guard is taken (strict r1 A-LOW).
    let explicit = explicit.map(|location| {
        let probe = super::record_store::writer_probe(&location);
        (location, probe)
    });
    let reg = registry.lock().unwrap_or_else(|p| p.into_inner());
    // An explicit archive_path belongs to another session or process, so the
    // registry gives no identity; it only says whether a worker of this
    // process may still be writing there.
    if let Some((location, probe)) = explicit {
        let writer = super::admission::writer_state_probed(
            probe,
            &location,
            env.own_process_dir,
            &reg,
        );
        return Ok((None, PriorSource::File { location, writer }));
    }
    let handle = reg.get(prior_handle_id);
    if handle.is_some_and(|h| h.status() == SubagentStatus::Running) {
        return Err(RuntimeError::Tool(format!(
            "Subagent '{}' is still running. Call subagent_collect first, \
                 or wait until it finishes.",
            prior_handle_id
        )));
    }
    let live = handle.map(|h| {
        let (watch_usage, thinking, cwd) = {
            let s = h.state.read().unwrap_or_else(|p| p.into_inner());
            let meta = s.archive_meta.as_ref();
            (
                meta.and_then(|m| m.watch_usage),
                meta.and_then(|m| m.thinking.clone()),
                meta.and_then(|m| m.cwd.clone()),
            )
        };
        (
            h.agent_name.clone(),
            h.model.clone(),
            h.system_prompt.clone(),
            h.timeout_secs,
            watch_usage,
            thinking,
            cwd,
        )
    });
    if let Some(h) = handle {
        let s = h.state.read().unwrap_or_else(|p| p.into_inner());
        if s.archive_error.is_some() || (s.archive_meta.is_some() && s.archive_path.is_none()) {
            if s.finalization_ready {
                if let Some(history) = &s.history {
                    let history = history.iter().map(|m| m.as_ref().clone()).collect();
                    // No archive file to date the run by: the recorded
                    // anchor, else time since the worker finished.
                    let cache_age = match s.cache_anchor_unix_ms {
                        Some(ms) => Some(super::archive::anchor_age(
                            ms,
                            std::time::SystemTime::now(),
                        )),
                        None => s.finished_at.map(|t| t.elapsed()),
                    };
                    return Ok((live, PriorSource::Memory { history, cache_age }));
                }
            }
            return Err(RuntimeError::Tool(format!(
                "Subagent '{}' has no finalized full history: {}",
                prior_handle_id,
                s.archive_error.as_deref().unwrap_or("archive pending")
            )));
        }
    }
    if let Some(path) = reg.archive_path(prior_handle_id) {
        // This process wrote it, under the same root; it still goes through
        // the same path rules and checks as a caller-supplied one. Its probe
        // stays under the guard: the root's canonicalization, then three
        // no-follow opens (root, process directory, lock file) and one
        // non-blocking shared flock. It never waits on a lock and reads no
        // record bytes, but these filesystem metadata operations have no hard
        // wall-time bound: a slow filesystem holds the guard while they run.
        let location = path
            .to_str()
            .ok_or(record_file::RecordFileError::NotUnderRoot)
            .and_then(|raw| record_file::locate(raw, env.root))
            .map_err(|e| file_error(prior_handle_id, e))?;
        let writer = super::admission::writer_state(&location, env.own_process_dir, &reg);
        return Ok((live, PriorSource::File { location, writer }));
    }
    match handle {
        Some(h) if h.is_tombstone() => Err(expired_context_error(prior_handle_id)),
        Some(h) => {
            let state = h.conversation_state();
            Ok((
                live,
                PriorSource::Text(if state.is_empty() {
                    h.partial_output()
                } else {
                    serde_json::to_string(&state).unwrap_or_else(|_| h.partial_output())
                }),
            ))
        }
        None => Err(RuntimeError::Tool(format!(
            "No subagent found with handle_id '{}'",
            prior_handle_id
        ))),
    }
}

/// [`prior_run_as`] for a `subagent_resume` call's resume (tests).
#[cfg(test)]
async fn prior_run(
    registry: &std::sync::Mutex<crate::runtime::subagent::SubagentRegistry>,
    prior_handle_id: &str,
    explicit: Option<RecordLocation>,
    instructions: &str,
    env: &ResumeEnv<'_>,
) -> Result<PriorRun> {
    let config = crate::config::SynapsConfig::default();
    let snapshot = ResumeSnapshot {
        config: &config,
        policy: ResumePolicy::FULL,
    };
    prior_run_as(registry, prior_handle_id, explicit, instructions, env, false, snapshot).await
}

/// Find and admit the prior run's context (§4 "Validation before
/// inference"), before the caller takes an ID or reserves anything. Its only
/// writes: a finished handle of this process may first retry that worker's
/// own failed archive write ([`super::archive::ArchiveRetry`], on a blocking
/// worker with no registry guard held). Admission reads files and asks the
/// backend (bounded). `by_daemon`: the daemon resumes a reload-stopped
/// worker, and `instructions` (its own words) are the whole next user turn;
/// otherwise the orchestrator's note opens it. `snapshot`: the spawn's
/// config (a v2 record's binding is asked for from it) and its cold-resume
/// policy.
async fn prior_run_as(
    registry: &std::sync::Mutex<crate::runtime::subagent::SubagentRegistry>,
    prior_handle_id: &str,
    explicit: Option<RecordLocation>,
    instructions: &str,
    env: &ResumeEnv<'_>,
    by_daemon: bool,
    snapshot: ResumeSnapshot<'_>,
) -> Result<PriorRun> {
    if explicit.is_none() {
        let retry = {
            let reg = registry.lock().unwrap_or_else(|p| p.into_inner());
            reg.get(prior_handle_id).and_then(|handle| {
                super::archive::ArchiveRetry::take(&handle.state, prior_handle_id)
                    .map(|retry| (retry, std::sync::Arc::clone(&handle.state)))
            })
        };
        if let Some((retry, state)) = retry {
            retry.run(state).await;
        }
    }
    let (live, source) = find_prior(registry, prior_handle_id, explicit, env)?;
    // A live handle's directory, if it still exists, is where the resumed
    // worker runs (`launch`), so its binding is asked for there.
    let live_dir = live
        .as_ref()
        .and_then(|(.., cwd)| cwd.clone())
        .filter(|dir| dir.is_dir());
    let (archived, history, cache_age, retrieval, warning, lineage) = match source {
        PriorSource::Memory { history, cache_age } => {
            let retrieval = super::lineage::admit_unlineaged(&history)
                .map_err(|e| file_error(prior_handle_id, e))?;
            (None, history, cache_age, retrieval, None, None)
        }
        PriorSource::File { location, writer } => {
            let (binding, config) = (env.binding, snapshot.config);
            let ask = || binding(config, live_dir.as_deref());
            let admitted = super::admission::admit_location(&location, writer, &ask)
                .await
                .map_err(|e| file_error(prior_handle_id, e))?;
            // Its recorded anchor, else the record file's mtime (written
            // at finalization, after the last request).
            let cache_age = super::archive::archive_cache_age(
                admitted.cache_anchor_unix_ms,
                Some(&location.path()),
                std::time::SystemTime::now(),
            );
            (
                Some(admitted.meta),
                admitted.history,
                cache_age,
                admitted.retrieval,
                admitted.warning,
                admitted.lineage,
            )
        }
        PriorSource::Text(prior_context) => {
            let (agent_name, model, system_prompt, timeout_secs, watch_usage, thinking, cwd) =
                live.expect("a text snapshot comes from a live handle");
            return Ok(PriorRun {
                agent_name,
                model,
                system_prompt,
                timeout_secs,
                cwd,
                thinking,
                messages: vec![Arc::new(json!({
                    "role": "user",
                    "content": format!(
                        "{instructions}\n\n---\n[Prior conversation context from handle {prior_handle_id}]\n{prior_context}"
                    )
                }))],
                watch_usage,
                retrieval: ResumeRetrieval::FullHistory,
                warning: None,
                lineage: None,
                compacted: None,
            });
        }
    };
    // A live handle's identity wins over the record's; its effort and
    // directory fall back to the record's.
    let (agent_name, model, system_prompt, timeout_secs, watch_usage, thinking, cwd) =
        match (live, archived) {
            (Some((agent, model, prompt, timeout, watch, thinking, cwd)), archived) => {
                let (archived_thinking, archived_cwd) =
                    archived.map_or((None, None), |m| (m.thinking, m.cwd));
                (
                    agent,
                    model,
                    prompt,
                    timeout,
                    watch,
                    thinking.or(archived_thinking),
                    cwd.or(archived_cwd),
                )
            }
            (None, Some(meta)) => (
                meta.agent_name,
                meta.model,
                meta.system_prompt,
                meta.timeout_secs,
                meta.watch_usage,
                meta.thinking,
                meta.cwd,
            ),
            (None, None) => unreachable!("in-memory history comes from a live handle"),
        };
    // Listed windows the store no longer has (evicted): the worker is told
    // once, in this resume's turn, after the note that opens it.
    let notice = match &retrieval {
        ResumeRetrieval::Owned { evicted, .. } => super::lineage::evicted_notice(evicted.len()),
        ResumeRetrieval::FullHistory => None,
    };
    // A cold, large history may open from a compact context (#315) only
    // where nothing else shapes the opening: a tool call's resume (the
    // daemon's reload resume keeps its own words), of a full history (an
    // owned-history lineage is already shortened), with no evicted-windows
    // notice. `launch` turns it off when the worker inherits the session's
    // context management (E9).
    let full = matches!(retrieval, ResumeRetrieval::FullHistory);
    let (messages, compacted) = match (by_daemon, notice) {
        (true, None) => (
            super::archive::resume_messages_saying(history, instructions),
            None,
        ),
        (true, Some(notice)) => (
            super::archive::resume_messages_saying(
                history,
                &format!("{instructions}\n\n{notice}"),
            ),
            None,
        ),
        (false, None) if full => super::archive::plan_resume_messages(
            history,
            instructions,
            cache_age,
            snapshot.policy,
        ),
        (false, None) => (super::archive::resume_messages(history, instructions), None),
        (false, Some(notice)) => (
            super::archive::resume_messages(history, &format!("{notice}\n\n{instructions}")),
            None,
        ),
    };
    Ok(PriorRun {
        agent_name,
        model,
        system_prompt,
        timeout_secs,
        cwd,
        thinking,
        messages,
        watch_usage,
        retrieval,
        warning,
        lineage,
        compacted,
    })
}

#[async_trait::async_trait]
impl Tool for SubagentResumeTool {
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "subagent_resume"
    }

    fn description(&self) -> &str {
        "Resume a finished, timed-out, failed or cancelled reactive subagent with new \
         instructions. The worker continues its own archived conversation (every tool \
         call and result) with the instructions as the next user turn, so it picks up \
         where it stopped. Works after the handle was collected or reaped. For a worker \
         from an earlier or crashed session, pass archive_path. A worker whose prompt \
         cache has expired and whose history is large resumes from a compact context \
         (task, last steps, final report) unless full_history is true or it inherits \
         the session's context management. Returns a new handle_id."
    }

    fn parameters(&self) -> Value {
        super::launch::with_rollover_param(json!({
            "type": "object",
            "properties": {
                "handle_id": {
                    "type": "string",
                    "description": "Handle ID of the completed subagent to resume (e.g. \"sa_3\")."
                },
                "instructions": {
                    "type": "string",
                    "description": "What to do next. Sent as the next user turn after the \
                                    worker's own prior conversation."
                },
                "archive_path": {
                    "type": "string",
                    "description": "Optional: a resume archive (.json under \
                                    ~/.synaps-cli/subagent-history/) of a worker from an \
                                    earlier or crashed session. handle_id is then only a label."
                },
                "full_history": {
                    "type": "boolean",
                    "description": "Optional (default false): replay the worker's full \
                                    conversation even when its prompt cache has expired. \
                                    Otherwise a cold history above \
                                    subagent_cold_resume_compact_tokens is resumed from a \
                                    compact context (original task, last steps, final \
                                    report), avoiding a full cache rewrite. A worker that \
                                    inherits the session's context management always \
                                    replays in full (it rolls over instead)."
                },
                "thinking": {
                    "type": "string",
                    "enum": ["low", "medium", "high", "xhigh", "max"],
                    "description": "Override worker effort for this resumed run. Omit to keep its recorded effort; legacy archives use subagent_thinking (default xhigh). Unsupported model levels are refused."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional wall-clock limit in seconds for the resumed run \
                                    (default: the prior run's); 0 = no limit. Positive values below 300s emit a warning; prefer at least 600s for API retries."
                }
            },
            "required": ["handle_id", "instructions"]
        }), super::launch::WORKER_ROLLOVER)
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let root = super::archive::archive_root();
        let binding = worker_binding(&ctx);
        let env = ResumeEnv {
            root: &root,
            own_process_dir: super::archive::process_dir_name(),
            binding: &binding,
            worker_tools: None,
            #[cfg(test)]
            planned: None,
        };
        self.execute_in(params, ctx, &env).await
    }
}

/// The production [`ResumeEnv::binding`]: the session's memory capability,
/// else the configured backend scoped to the worker's directory (the prior
/// run's, else the session's, else the process's), as
/// `apply_subagent_runtime_policy` scopes the worker's own.
fn worker_binding(
    ctx: &ToolContext,
) -> impl Fn(&crate::config::SynapsConfig, Option<&std::path::Path>) -> MemoryBinding + Send + Sync
{
    let configured = ctx.capabilities.memory_backend.clone();
    let session_cwd = ctx.capabilities.cwd.clone();
    move |config, prior_dir| {
        configured.clone().unwrap_or_else(|| {
            MemoryBinding::from_config_with_cwd(
                &config.memory_backend,
                prior_dir
                    .map(std::path::Path::to_path_buf)
                    .or_else(|| session_cwd.clone())
                    .or_else(|| std::env::current_dir().ok()),
            )
        })
    }
}

/// Who started a resume.
#[derive(Clone, Copy)]
enum ResumeCaller<'a> {
    /// A `subagent_resume` tool call: its child route is its tool call's.
    Tool,
    /// The daemon, resuming a reload-stopped worker after the reload: no
    /// tool call, so the session's own child route.
    Daemon {
        route: Option<&'a crate::session::child_events::ChildRoute>,
    },
}

/// What a resume launch came to.
enum Launched {
    /// The run started; `reply` is the tool's answer.
    Started {
        handle_id: String,
        context_tokens: u64,
        /// Listed windows the store evicted (0 = none).
        evicted_windows: u64,
        reply: String,
    },
    /// Refused without an error (a spent watch lease): the tool's answer.
    Refused(String),
}

/// The daemon's own resume of `prior_handle_id`, a worker the reload
/// stopped (`SessionActor::adopt_workers`). The same launch as a
/// `subagent_resume` call, on a context the session built for it
/// (`Runtime::daemon_tool_context`), with the daemon's instructions
/// ([`super::finalize::DAEMON_RESUME_INSTRUCTIONS`]) and the run's archived
/// timeout. The worker's tools are the worker registry (or the owned-history
/// registry its archive grants), exactly as for a tool call's resume: a
/// resume never takes a turn's activations or MCP/extension leases, so none
/// is needed outside a turn. `Ok((new handle, context tokens, evicted
/// windows))`, or why nothing was started.
pub(crate) async fn resume_for_daemon(
    ctx: ToolContext,
    prior_handle_id: &str,
    route: Option<&crate::session::child_events::ChildRoute>,
) -> std::result::Result<(String, u64, u64), String> {
    let root = super::archive::archive_root();
    let binding = worker_binding(&ctx);
    let env = ResumeEnv {
        root: &root,
        own_process_dir: super::archive::process_dir_name(),
        binding: &binding,
        worker_tools: None,
        #[cfg(test)]
        planned: None,
    };
    resume_for_daemon_in(ctx, prior_handle_id, route, &env).await
}

/// [`resume_for_daemon`] against explicit archive locations.
async fn resume_for_daemon_in(
    ctx: ToolContext,
    prior_handle_id: &str,
    route: Option<&crate::session::child_events::ChildRoute>,
    env: &ResumeEnv<'_>,
) -> std::result::Result<(String, u64, u64), String> {
    let params = json!({
        "handle_id": prior_handle_id,
        "instructions": super::finalize::DAEMON_RESUME_INSTRUCTIONS,
    });
    match launch(params, ctx, env, ResumeCaller::Daemon { route }).await {
        Ok(Launched::Started {
            handle_id,
            context_tokens,
            evicted_windows,
            ..
        }) => Ok((handle_id, context_tokens, evicted_windows)),
        Ok(Launched::Refused(text)) => Err(text),
        // The tool's own words, without the tool-failure label.
        Err(RuntimeError::Tool(text)) => Err(text),
        Err(error) => Err(error.to_string()),
    }
}

impl SubagentResumeTool {
    /// [`SubagentResumeTool::execute`] against explicit archive locations and
    /// worker binding (tests use a temporary root).
    pub(super) async fn execute_in(
        &self,
        params: Value,
        ctx: ToolContext,
        env: &ResumeEnv<'_>,
    ) -> Result<String> {
        // A worker the daemon resumed after a reload already continues as
        // its new run: a second resume would start a duplicate. (With an
        // explicit archive_path the id is only a label.)
        if let (Some(prior), None, Some(registry)) = (
            params["handle_id"].as_str(),
            params["archive_path"].as_str(),
            ctx.capabilities.subagent_registry.as_ref(),
        ) {
            let resumed = registry.lock().unwrap_or_else(|p| p.into_inner()).resumed_as(prior);
            match resumed {
                Some(Some(new)) => {
                    return Err(RuntimeError::Tool(format!(
                        "Subagent '{prior}' was stopped by a daemon reload and already resumed as {new} \
                         by the daemon: wait for or collect {new} instead. Nothing was started."
                    )))
                }
                Some(None) => {
                    return Err(RuntimeError::Tool(format!(
                        "Subagent '{prior}' was stopped by a daemon reload and is being resumed by the \
                         daemon; its new handle is announced shortly. Nothing was started."
                    )))
                }
                None => {}
            }
        }
        match launch(params, ctx, env, ResumeCaller::Tool).await? {
            Launched::Started { reply, .. } => Ok(reply),
            Launched::Refused(text) => Ok(text),
        }
    }
}

/// The launch half of a resume: admit the prior run, then start its
/// continuation (or refuse before anything exists).
async fn launch(
    params: Value,
    ctx: ToolContext,
    env: &ResumeEnv<'_>,
    caller: ResumeCaller<'_>,
) -> Result<Launched> {
    {
        let prior_handle_id = params["handle_id"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'handle_id' parameter".to_string()))?
            .to_string();

        let instructions = params["instructions"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'instructions' parameter".to_string()))?
            .to_string();
        // The caller's opt-out of worker rollover (§2), checked before
        // anything else happens once worker rollover is built in.
        let rollover = super::launch::context_rollover_param(&params, super::launch::WORKER_ROLLOVER)
            .map_err(RuntimeError::Tool)?;

        let registry = ctx.capabilities.subagent_registry.as_ref().ok_or_else(|| {
            RuntimeError::Tool("SubagentRegistry not available on this ToolContext".to_string())
        })?;

        let explicit = match params["archive_path"].as_str() {
            Some(raw) => Some(
                record_file::locate(raw, env.root).map_err(|e| file_error(&prior_handle_id, e))?,
            ),
            None => None,
        };

        // One config snapshot for the whole spawn (`spawn_config`).
        let config = super::spawn_config();
        // The parent's live context settings at this call (§2): a resume
        // takes a fresh snapshot, never its predecessor's.
        let worker_context = ctx.capabilities.worker_context.clone();
        let wants_rollover = super::launch::wants_rollover(worker_context.as_ref(), rollover);
        // E9: a worker that inherits the session's context management keeps
        // its context small by rolling over; the cold-resume compaction
        // (#315) yields to it and runs only for a worker without it.
        #[cfg(test)]
        let wants_rollover = wants_rollover || env.planned.is_some();
        let policy = ResumePolicy {
            full_history: params["full_history"].as_bool().unwrap_or(false),
            compact_tokens: if wants_rollover {
                0
            } else {
                config.subagent_cold_resume_compact_tokens
            },
        };
        let by_daemon = matches!(caller, ResumeCaller::Daemon { .. });
        let snapshot = ResumeSnapshot {
            config: &config,
            policy,
        };
        let prior = prior_run_as(
            registry,
            &prior_handle_id,
            explicit,
            &instructions,
            env,
            by_daemon,
            snapshot,
        )
        .await?;
        let thinking =
            super::resolve_subagent_thinking(&params, prior.thinking.as_deref(), &config)?;
        let compacted = prior.compacted;
        if let Some(c) = compacted {
            tracing::info!(
                prior_handle_id = %prior_handle_id,
                messages = c.messages,
                est_tokens = c.est_tokens,
                kept_est_tokens = c.kept_est_tokens,
                cache_age_secs = ?c.cache_age_secs,
                "subagent_resume: cold cache, resuming from a compact context"
            );
        }
        // The resumed worker keeps the directory it was started in (its
        // history refers to paths there); else the resuming session's.
        let prior_dir = prior.cwd.filter(|dir| dir.is_dir());
        let mut workspace = super::WorkerWorkspace::from_capabilities(&ctx.capabilities);
        if let Some(prior_cwd) = prior_dir.clone() {
            workspace.cwd = Some(prior_cwd);
        }
        // A shortened history is retrievable only through the owned-history
        // tools, on the very binding its lineage was validated against. The
        // worker's registry (no other memory capability) is built here, so
        // a failure refuses before an ID, reservation or handle exists.
        let (memory_backend, owned_tools, owned_grant) = match &prior.retrieval {
            ResumeRetrieval::Owned { grant, .. } => {
                let base = match env.worker_tools {
                    Some(tools) => tools.clone(),
                    None => super::worker_tools().await,
                };
                let tools = super::retrieval::owned_worker_registry(&base, Arc::clone(grant))
                    .map_err(|e| file_error(&prior_handle_id, e))?;
                (Some(grant.binding().clone()), Some(tools), Some(Arc::clone(grant)))
            }
            ResumeRetrieval::FullHistory => (ctx.capabilities.memory_backend.clone(), None, None),
        };
        // Listed windows the store evicted: the worker is told in its opening
        // turn; the parent gets the count (reply warning, status, collect,
        // completion), never their IDs.
        let evicted_windows = super::lineage::evicted_count(&prior.retrieval) as u64;
        let warning = prior.warning;
        let lineage = prior.lineage;
        let prior_watch = prior.watch_usage;
        let (agent_name, model, system_prompt, initial_messages) = (
            prior.agent_name,
            prior.model,
            prior.system_prompt,
            prior.messages,
        );
        // Context admission (request_size): refused above the line before any
        // id, reservation, grant, handle or event exists.
        let admitted = super::request_size::admit_resume(
            &ctx,
            &model,
            &system_prompt,
            None,
            &initial_messages,
            &prior_handle_id,
        )
        .await?;
        let timeout_secs = params["timeout"].as_u64().unwrap_or(prior.timeout_secs);
        let timeout_warning = short_timeout_warning(timeout_secs);
        if let Some(warning) = timeout_warning {
            tracing::warn!(timeout_secs, warning, "short subagent resume timeout");
        }

        // The inherited prior model is still re-authorized as an explicit exact
        // identity; a stale handle cannot bypass current session policy.
        let label = agent_name.clone();
        let task_preview: String = instructions.chars().take(80).collect();
        let task_full = instructions.clone();
        let subagent_id = NEXT_SUBAGENT_ID.fetch_add(1, Ordering::Relaxed);
        let handle_id = format!("sa_{}", subagent_id);
        let orchestration = ctx
            .capabilities
            .orchestration
            .as_ref()
            .ok_or_else(|| RuntimeError::Tool("delegation policy unavailable".into()))?;
        orchestration
            .reserve_delegation(&handle_id, ctx.capabilities.delegation_parent.as_deref())
            .map_err(|reason| {
                RuntimeError::Tool(format!("delegation tree budget denied: {reason:?}"))
            })?;
        let decision = orchestration
            .resolve_and_authorize(&handle_id, Some(&model))
            .map_err(|error| {
                // Refused before launch: release the edge and drop its owner entry.
                orchestration.abandon_delegation(&handle_id);
                RuntimeError::Tool(error.to_string())
            })?;
        let model = decision.model.as_str().to_owned();
        // Small-watcher admission: the new instructions are judged like a
        // start's task, and a watch worker continues its lease, so a spent
        // lease refuses the resume.
        let watch_admitted = match super::watch_admission::admit_dispatch(
            "subagent_resume",
            &super::watch_admission::Dispatch {
                request: super::watch_admission::WatchRequest {
                    name: Some(&label),
                    task: &instructions,
                    explicit_write_policy: None,
                },
                requested_model: None,
                resolved_model: &model,
            },
            prior_watch,
            None,
        )
        .await
        {
            Ok(watch_admitted) => watch_admitted,
            Err(refusal) => {
                orchestration.rollback(&handle_id);
                orchestration
                    .release_delegation(&handle_id, ctx.capabilities.delegation_parent.as_deref());
                return Ok(Launched::Refused(refusal));
            }
        };
        let lease = watch_admitted.watch;
        let timeout_secs = lease.map_or(timeout_secs, |(l, _)| l.clamp_timeout(timeout_secs));
        let codex_parent_plan = ctx.capabilities.codex_parent_plan.clone();
        let session_allow_all = ctx.capabilities.session_allow_all.clone();
        let telemetry = ctx.capabilities.telemetry.clone();
        let telemetry = telemetry.map(|t| t.for_worker(&label, &handle_id));
        // Child spend: a new run's lease. Refused before launch (the
        // delegating run was sealed, or the session is ending): undo the
        // authorization and the reservation.
        let spend_lease = super::spend_lease(&ctx, &handle_id, &label).map_err(|refused| {
            orchestration.rollback(&handle_id);
            orchestration.abandon_delegation(&handle_id);
            RuntimeError::Tool(refused.to_string())
        })?;
        let worker_hooks = super::WorkerHooks::of(&ctx.capabilities, &handle_id);
        // Worker rollover (on since slice 9, when the run wants it): the
        // run's records, on the binding the worker runs on (for an owned
        // grant, the one its predecessor was admitted against).
        let site = super::launch::wants_rollover(worker_context.as_ref(), rollover).then(|| {
            super::oneshot::RecordSite::at(env.root.to_path_buf(), env.own_process_dir.to_owned())
        });
        #[cfg(test)]
        let site = env.planned.clone().or(site);
        let plan = site.map(|site| super::launch::LaunchPlan {
            root: site.root,
            process_dir: site.process_dir,
            handle_id: handle_id.clone(),
            session: ctx.capabilities.session_id.clone(),
            binding: memory_backend
                .clone()
                .unwrap_or_else(|| (env.binding)(&config, prior_dir.as_deref())),
            meta: super::record::WorkerMeta {
                agent_name: label.clone(),
                model: model.clone(),
                system_prompt: system_prompt.clone(),
                timeout_secs,
                thinking: Some(thinking.as_str().into()),
                watch_usage: lease.map(|(_, used)| used.into()),
            },
            lineage,
            ledger: Default::default(),
            context: super::launch::worker_context(worker_context.as_ref(), &model),
            // A v2 run's windows grow the grant it was admitted with (8g);
            // without one, its lineage starts a grant of its own.
            grant: owned_grant,
            tools: None,
            evicted_windows,
        });

        tracing::info!(
            "subagent_resume: dispatching '{}' (id={}, resumed_from={}) model={}",
            label,
            handle_id,
            prior_handle_id,
            model
        );

        let state = Arc::new(RwLock::new(SubagentState::new()));
        state.write().unwrap().archive_meta = Some(crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: label.clone(),
            model: model.clone(),
            system_prompt: system_prompt.clone(),
            timeout_secs,
            cwd: workspace
                .cwd
                .clone()
                .or_else(|| std::env::current_dir().ok()),
            thinking: Some(thinking.as_str().into()),
            watch_usage: lease.map(|(_, used)| used),
        });

        state.write().unwrap().archive_dir = Some(super::archive::process_dir());
        // Visible from the first status, whether or not a launch plan runs.
        state.write().unwrap().evicted_windows = evicted_windows;

        // Keep the inherited checkpoint even if runtime startup fails before emitting history.
        state.write().unwrap().history = Some(initial_messages.clone());

        let (steer_tx, steer_rx) = mpsc::unbounded_channel::<String>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (result_tx, result_rx) = oneshot::channel::<SubagentResult>();

        if let Some(ref tx) = ctx.channels.tx_events {
            let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentStart {
                subagent_id,
                agent_name: label.clone(),
                task_preview: task_preview.clone(),
            }));
        }

        let state_t = Arc::clone(&state);
        let task_full_a = task_full.clone();
        let label_inner = label.clone();
        let model_inner = model.clone();
        let tx_events_inner = ctx.channels.tx_events.clone();
        let start_time = std::time::Instant::now();
        let parent_queue = ctx.capabilities.event_queue.clone();
        let handle_id_inner = handle_id.clone();
        let prior_handle_for_finalizer = prior_handle_id.clone();
        let child_parent_id = handle_id.clone();
        let child_route = match caller {
            ResumeCaller::Tool => crate::session::child_events::worker_route(
                ctx.channels.tx_delta.as_ref(),
                &handle_id,
            ),
            ResumeCaller::Daemon { route } => route.map(|route| route.for_worker(&handle_id)),
        };

        // ── Build and register handle BEFORE spawning ─────────────────────────
        let system_prompt_for_handle = system_prompt.clone();
        let handle = SubagentHandle::new(
            handle_id.clone(),
            subagent_id,
            label.clone(),
            task_preview,
            model.clone(),
            system_prompt_for_handle,
            timeout_secs,
            Arc::clone(&state),
            Some(steer_tx),
            Some(shutdown_tx),
            Some(result_rx),
        )
        .with_authorization(&decision);
        {
            let mut reg = registry.lock().unwrap();
            reg.register_with_cancellation(handle, ctx.capabilities.launch_cancel.as_ref());
        }

        let orchestration_for_worker = Arc::clone(orchestration);
        let parent_for_worker = ctx.capabilities.delegation_parent.clone();
        if let Err(error) = orchestration.mark_starting(&handle_id) {
            orchestration.rollback(&handle_id);
            orchestration
                .release_delegation(&handle_id, ctx.capabilities.delegation_parent.as_deref());
            return Err(RuntimeError::Tool(error));
        }
        // ── Spawn subagent thread ──────────────────────────────────────────────
        let first_request =
            admitted.first_request(super::request_size::Kind::Resume, &handle_id, &model);
        let thread_handle = std::thread::spawn(move || {
            // Pre-clone for finalizer — catch_unwind moves state_t and label_inner
            let state_for_finalizer = Arc::clone(&state_t);
            let label_for_finalizer = label_inner.clone();
            let orchestration_for_runtime = Arc::clone(&orchestration_for_worker);

            let mut pending_result = None;
            let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => {
                        state_t.write().unwrap().status =
                            SubagentStatus::Failed("runtime initialization failed".into());
                        return;
                    }
                };

                let state_a = Arc::clone(&state_t);
                let label_a = label_inner.clone();
                let model_a = model_inner.clone();
                let tx_events_a = tx_events_inner.clone();
                let task_for_timeout = task_full_a.clone();
                let task_for_complete = task_full_a;

                let outcome: std::result::Result<SubagentResult, String> =
                    rt.block_on(async move {
                        // Host-built worker (shared client/creds/token cache, cached
                        // registry) or the legacy fresh runtime — see `spawn_runtime`.
                        let mut runtime = match super::spawn_runtime().await {
                            Ok(r) => r,
                            Err(_) => return Err("subagent runtime initialization failed".into()),
                        };

                        // Apply subagent spawn policy: worker role, 5m cache TTL, worker
                        // turn budget. Subagents are short-lived one-shots — paying the 1h
                        // write premium (~2× input price) on them is unrecoverable waste
                        // (~$0.23 per 10-spawn fan-out). (#110)
                        super::apply_subagent_runtime_policy(
                            &mut runtime,
                            &config,
                            memory_backend.as_ref(),
                            session_allow_all.as_ref(),
                            telemetry.as_ref(),
                            &workspace,
                        );
                        // The owned registry built at admission replaces
                        // the worker registry; the operator's disabled tools
                        // stay out of it, as out of every worker registry.
                        if let Some(mut tools) = owned_tools {
                            super::disable_configured_tools(&mut tools, &config);
                            runtime.set_tools(tools);
                        }
                        let mut lease_binds_rounds = false;
                        if let Some((lease, _)) = lease {
                            let mut budget = runtime.turn_budget().clone();
                            lease_binds_rounds = lease.apply_to(&mut budget);
                            runtime.set_turn_budget(budget);
                        }
                        // After the policy: its transform view (or the
                        // `subagent_transform_hooks = off` detach) is kept.
                        worker_hooks.apply(&mut runtime, &config);
                        runtime.set_system_prompt(super::compose_system_prompt(
                            system_prompt,
                            runtime.memory_backend_is_axel(),
                        ));
                        runtime.set_model(model_a.clone());
                        super::fit_subagent_context_window(&mut runtime);
                        super::apply_codex_worker_reasoning(
                            &mut runtime,
                            codex_parent_plan.as_ref(),
                        );
                        super::apply_subagent_thinking(&mut runtime, thinking)?;
                        runtime
                            .install_worker_orchestration(Arc::clone(&orchestration_for_runtime));
                        runtime.set_delegation_parent(Some(child_parent_id.clone()));
                        // Rolled-over context goes beside this worker's
                        // resume archive (see `worker_context`).
                        let archive_dir = state_a
                            .read()
                            .unwrap_or_else(|p| p.into_inner())
                            .archive_dir
                            .clone()
                            .unwrap_or_else(super::archive::process_dir);
                        super::attach_worker_transcripts(
                            &mut runtime,
                            archive_dir,
                            &child_parent_id,
                            Some(Arc::clone(&state_a)),
                            &initial_messages,
                            tx_events_a.clone(),
                            subagent_id,
                            &label_a,
                        );
                        runtime.set_child_route(child_route.clone());
                        super::install_spend(&mut runtime, spend_lease);

                        let cancel = crate::CancellationToken::new();
                        let cancel_inner = cancel.clone();
                        tokio::spawn(async move {
                            let _ = shutdown_rx.await;
                            cancel_inner.cancel();
                        });

                        let cancel_on_timeout = cancel.clone();
                        // A run with a plan shares one run clock with its
                        // runtime (§5); without one nothing changes.
                        let run_clock = super::launch::arm_run_clock(
                            plan.as_ref(),
                            &mut runtime,
                            timeout_secs,
                        );
                        // Its requests book into the run's ledger only once
                        // the run is a v2 run (§6).
                        super::launch::arm_request_ledger(plan.as_ref(), &mut runtime);
                        // A v2 run retrieves its own lineage's history
                        // only (8g): without an admitted grant, its
                        // registry as configured, for its own grant's
                        // tools to join.
                        let mut plan = plan;
                        super::launch::arm_owned_history(plan.as_mut(), &runtime, &config).await;
                        let drive_outcome = {
                        let runtime = &mut runtime;
                        // A v2 run's runtime is bound to its run and gets
                        // the parent's settings resolved for it (§2, §7.2)
                        // before its first inference; a run without
                        // rollover gets nothing.
                        let start = move |opening: Vec<crate::SharedMessage>,
                                          start: Option<super::launch::WorkerStart>| async move {
                            if let Some(start) = start {
                                super::launch::start_v2_runtime(runtime, start, &opening);
                            }
                            runtime.run_stream_with_messages(
                                opening,
                                cancel,
                                Some(steer_rx),
                                None,
                                false,
                            )
                            .await
                        };

                        let drive = super::drive::WorkerDrive {
                            state: Arc::clone(&state_a),
                            tx_events: tx_events_a.clone(),
                            subagent_id,
                            label: label_a.clone(),
                            model: model_a.clone(),
                            timeout_secs,
                            run_clock,
                            cancel: cancel_on_timeout,
                            task: task_for_timeout,
                            first_request: Some(first_request),
                            lease,
                            lease_binds_rounds,
                            shape: super::drive::DriveShape::Reactive,
                        };
                        super::launch::run_worker(drive, initial_messages, plan, start).await
                        };
                        // Date this run's cache for the next resume (cold check).
                        super::archive::record_cache_anchor(&state_a, runtime.cache_anchor());
                        drive_outcome
                    });

                match outcome {
                    Ok(sa_result) => {
                        {
                            let mut s = state_t.write().unwrap();
                            if matches!(s.status, SubagentStatus::Running) && !s.cancel_requested {
                                s.status = SubagentStatus::Completed;
                                s.conversation_state = vec![
                                    serde_json::json!({"role": "user", "content": task_for_complete.clone()}),
                                    serde_json::json!({"role": "assistant", "content": sa_result.text.clone()}),
                                ];
                            }
                        }
                        pending_result = Some(sa_result);
                    }
                    Err(e) => {
                        state_t.write().unwrap().status = SubagentStatus::Failed(e);
                    }
                }
            }));

            if let Err(panic_info) = panic_result {
                let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                tracing::error!("Resumed subagent thread panicked: {}", msg);
                state_t.write().unwrap_or_else(|p| p.into_inner()).status =
                    SubagentStatus::Failed(format!("panic: {}", msg));
            }

            // ── Terminal finalizer — exactly once, outside catch_unwind ────────
            // Covers all paths: Ok, Err, timeout, panic, early tokio-build failure.
            // Sets data.resumed_from so parent can correlate with the prior handle.
            super::finalize::finalize_subagent(
                &state_for_finalizer,
                parent_queue.as_ref(),
                &handle_id_inner,
                subagent_id,
                &label_for_finalizer,
                start_time,
                Some(&prior_handle_for_finalizer),
            );
            // A cancelled worker publishes no wake, so nothing would ever
            // prompt a collect: retire it before any terminal publication.
            super::finalize::retire_if_cancelled(
                &state_for_finalizer,
                &orchestration_for_worker,
                &handle_id_inner,
            );
            // All terminal publications follow the recovery/finalization barrier.
            if let Some(ref tx) = tx_events_inner {
                let s = state_for_finalizer
                    .read()
                    .unwrap_or_else(|p| p.into_inner());
                let preview = match s.status.failure_reason() {
                    Some(reason) => format!("ERROR: {reason}"),
                    None => s.partial_text.chars().take(120).collect(),
                };
                let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentDone {
                    subagent_id,
                    agent_name: label_for_finalizer.clone(),
                    result_preview: preview,
                    duration_secs: start_time.elapsed().as_secs_f64(),
                }));
            }
            if let Some(result) = pending_result {
                let _ = result_tx.send(result);
            }
            orchestration_for_worker
                .release_delegation(&handle_id_inner, parent_for_worker.as_deref());
        });

        // ── Wire thread handle into the already-registered entry ─────────────
        {
            let mut reg = registry.lock().unwrap();
            if let Some(h) = reg.get_mut(&handle_id) {
                h.set_thread_handle(thread_handle);
            }
        }
        if let Err(error) = orchestration.mark_running(&handle_id) {
            let mut handle = registry.lock().unwrap().remove(&handle_id);
            if let Some(handle) = handle.as_mut() {
                handle.cancel();
            }
            if let Some(handle) = handle {
                let _ = handle.collect().await;
            }
            orchestration.rollback(&handle_id);
            orchestration
                .release_delegation(&handle_id, ctx.capabilities.delegation_parent.as_deref());
            return Err(RuntimeError::Tool(error));
        }
        // The thread may have been cancelled and finalized before it was
        // marked running; its own retire attempt was then too early.
        super::finalize::retire_if_cancelled(&state, orchestration, &handle_id);

        let context = if compacted.is_some() {
            "compacted"
        } else {
            "full"
        };
        let mut reply = json!({
            "handle_id":    handle_id,
            "resumed_from": prior_handle_id,
            "warning": timeout_warning,
            "agent_name":   label,
            "status":       "running",
            "context":      context
        });
        if let Some(c) = compacted {
            reply["compacted"] = json!(c);
        }
        admitted.add_reply_fields(&mut reply);
        // A crash-checkpoint resume's warning, then the evicted windows'
        // count-only note, then a short timeout's, joined with "; ".
        let evicted_note = super::lineage::evicted_parent_note(evicted_windows as usize);
        let parts: Vec<&str> = [warning, evicted_note.as_deref(), timeout_warning]
            .into_iter()
            .flatten()
            .collect();
        if !parts.is_empty() {
            reply["warning"] = json!(parts.join("; "));
        }
        Ok(Launched::Started {
            handle_id,
            context_tokens: admitted.size.context_tokens,
            evicted_windows,
            reply: reply.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::subagent::{reap_finished_with_ttl, SubagentRegistry};
    use crate::tools::test_helpers::create_tool_context;
    use std::sync::Mutex;
    use std::time::Duration;

    /// A temporary archive root with one process directory (not this
    /// process's) for record files.
    struct Layout {
        _tmp: tempfile::TempDir,
        root: std::path::PathBuf,
        dir: std::path::PathBuf,
    }

    fn layout() -> Layout {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("subagent-history");
        let dir = root.join(super::super::test_fixtures::PROCESS_DIR);
        Layout {
            _tmp: tmp,
            root,
            dir,
        }
    }

    /// Another process's directory name: never the fixture's.
    const OTHER_PROCESS_DIR: &str = "1-00000000-0000-4000-8000-000000000001";

    fn no_binding(
        _: &crate::config::SynapsConfig,
        _: Option<&std::path::Path>,
    ) -> MemoryBinding {
        panic!("a legacy resume never asks for a memory binding")
    }

    /// The spawn's config snapshot in these tests: the defaults.
    static CONFIG: std::sync::LazyLock<crate::config::SynapsConfig> =
        std::sync::LazyLock::new(crate::config::SynapsConfig::default);

    /// A resume under `policy` on the default config.
    fn snapshot(policy: ResumePolicy) -> ResumeSnapshot<'static> {
        ResumeSnapshot {
            config: &CONFIG,
            policy,
        }
    }

    fn env(layout: &Layout) -> ResumeEnv<'_> {
        ResumeEnv {
            root: &layout.root,
            own_process_dir: OTHER_PROCESS_DIR,
            binding: &no_binding,
            worker_tools: None,
            planned: None,
        }
    }

    fn locate(layout: &Layout, path: &std::path::Path) -> RecordLocation {
        record_file::locate(path.to_str().unwrap(), &layout.root).unwrap()
    }

    fn finished_handle(
        id: &str,
        status: SubagentStatus,
        archive_path: Option<std::path::PathBuf>,
    ) -> SubagentHandle {
        let state = Arc::new(RwLock::new(SubagentState::new()));
        {
            let mut s = state.write().unwrap();
            s.status = status;
            s.partial_text = "prior text".into();
            s.conversation_state = vec![json!({"role": "assistant", "content": "prior text"})];
            s.finished_at = Some(std::time::Instant::now());
            s.archive_path = archive_path;
        }
        let (steer_tx, _steer_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = oneshot::channel();
        let (_result_tx, result_rx) = oneshot::channel();
        SubagentHandle::new(
            id.into(),
            1,
            "live-label".into(),
            "task".into(),
            "anthropic/claude-sonnet-4-6".into(),
            "live system".into(),
            30,
            state,
            Some(steer_tx),
            Some(shutdown_tx),
            Some(result_rx),
        )
    }

    fn archive_in(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
        let meta = crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: "archived-label".into(),
            model: "anthropic/claude-opus-5-5".into(),
            system_prompt: "archived system".into(),
            timeout_secs: 3500,
            cwd: None,
            thinking: Some("low".into()),
            watch_usage: None,
        };
        let history: Vec<crate::SharedMessage> = vec![
            Arc::new(json!({"role": "user", "content": "build the plate"})),
            Arc::new(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "render"}}
            ]})),
            Arc::new(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "rendered 300 frames"}
            ]})),
        ];
        super::super::archive::write_archive(
            dir,
            id,
            &meta,
            "timed_out",
            Some("worker timed out"),
            "report",
            &history,
        )
        .unwrap()
    }

    fn assert_continues_archive(prior: &PriorRun, instructions: &str) {
        assert_eq!(
            prior.messages.len(),
            3,
            "archived turns replayed, instructions merged"
        );
        assert_eq!(prior.messages[0]["content"], "build the plate");
        let last = &prior.messages[2]["content"];
        assert_eq!(last[0]["content"], "rendered 300 frames");
        assert!(last[1]["text"].as_str().unwrap().ends_with(instructions));
    }

    /// A worker the daemon resumed after a reload continues as its new run:
    /// `subagent_resume` of the old id is refused, naming the new one, and
    /// starts nothing (mutant: drop the alias check in `execute_in`).
    #[tokio::test]
    async fn a_worker_the_daemon_resumed_is_not_resumed_again_by_hand() {
        let layout = layout();
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        {
            let mut reg = registry.lock().unwrap();
            reg.begin_daemon_resume("sa_70");
            reg.finish_daemon_resume("sa_70", Some("sa_71"));
            reg.begin_daemon_resume("sa_72");
        }
        for (old, expected) in [
            ("sa_70", "already resumed as sa_71 by the daemon"),
            ("sa_72", "is being resumed by the daemon"),
        ] {
            let mut ctx = create_tool_context();
            ctx.capabilities.subagent_registry = Some(registry.clone());
            let err = SubagentResumeTool
                .execute_in(json!({"handle_id": old, "instructions": "go on"}), ctx, &env(&layout))
                .await
                .expect_err("refused");
            let text = err.to_string();
            assert!(text.contains(expected) && text.contains("Nothing was started"), "{text}");
        }
        assert!(registry.lock().unwrap().list_active().is_empty());
    }

    /// The daemon's resume says itself who resumed the worker: its words
    /// are the whole next user turn, with no orchestrator note (mutant:
    /// `prior_run_as` ignores `by_daemon`).
    #[tokio::test]
    async fn a_daemon_resume_opens_with_the_daemons_words() {
        let layout = layout();
        let path = archive_in(&layout.dir, "sa_73");
        let registry = Mutex::new(SubagentRegistry::new());
        registry.lock().unwrap().adopt_carried(&crate::runtime::subagent::CarriedWorker {
            id: "sa_73".into(),
            status: "cancelled".into(),
            archive_path: Some(path),
            stopped_by_reload: true,
            wake: true,
            ..Default::default()
        });
        let words = super::super::finalize::DAEMON_RESUME_INSTRUCTIONS;
        let full = snapshot(ResumePolicy::FULL);
        let prior = prior_run_as(&registry, "sa_73", None, words, &env(&layout), true, full)
            .await
            .unwrap();
        assert_continues_archive(&prior, words);
        assert_eq!(prior.messages[2]["content"][1]["text"], words);
        let by_hand = prior_run(&registry, "sa_73", None, "go on", &env(&layout)).await.unwrap();
        let text = by_hand.messages[2]["content"][1]["text"].as_str().unwrap();
        assert!(text.starts_with("[Resumed by the orchestrator."), "{text}");
    }

    #[tokio::test]
    async fn subagent_thinking_resume_uses_archived_effort_after_reload() {
        let layout = layout();
        let path = archive_in(&layout.dir, "sa_74");
        let registry = Mutex::new(SubagentRegistry::new());
        registry.lock().unwrap().adopt_carried(&crate::runtime::subagent::CarriedWorker {
            id: "sa_74".into(), status: "cancelled".into(), archive_path: Some(path),
            stopped_by_reload: true, wake: true, ..Default::default()
        });
        let full = snapshot(ResumePolicy::FULL);
        let prior = prior_run_as(&registry, "sa_74", None, "continue", &env(&layout), true, full)
            .await
            .unwrap();
        assert_eq!(prior.thinking.as_deref(), Some("low"));
        let cfg = crate::config::load_config_from_str("subagent_thinking = max\n");
        let retained = super::super::resolve_subagent_thinking(&json!({}), prior.thinking.as_deref(), &cfg).unwrap();
        let changed = super::super::resolve_subagent_thinking(&json!({"thinking":"xhigh"}), prior.thinking.as_deref(), &cfg).unwrap();
        let mut runtime = crate::Runtime::new_headless();
        runtime.set_model("openai-codex/gpt-6-astra".into());
        super::super::apply_subagent_thinking(&mut runtime, retained).unwrap();
        assert_eq!(runtime.reasoning_level().as_str(), "low");
        super::super::apply_subagent_thinking(&mut runtime, changed).unwrap();
        assert_eq!(runtime.reasoning_level().as_str(), "xhigh");
    }

    // Upstream's (#319): an archive's effort survives a config change and
    // yields to an explicit override.
    #[tokio::test]
    async fn subagent_thinking_resume_archive_survives_config_change_and_override() {
        let layout = layout();
        let path = archive_in(&layout.dir, "sa_75");
        let registry = Mutex::new(SubagentRegistry::new());
        let prior = prior_run(&registry, "sa_75", Some(locate(&layout, &path)), "continue", &env(&layout))
            .await
            .unwrap();
        assert_eq!(prior.thinking.as_deref(), Some("low"));
        let cfg = crate::config::load_config_from_str("subagent_thinking = max\n");
        let retained = super::super::resolve_subagent_thinking(&json!({}), prior.thinking.as_deref(), &cfg).unwrap();
        assert_eq!(retained.as_str(), "low");
        let overridden = super::super::resolve_subagent_thinking(&json!({"thinking":"xhigh"}), prior.thinking.as_deref(), &cfg).unwrap();
        let mut runtime = crate::Runtime::new_headless();
        runtime.set_model("openai-codex/gpt-6-astra".into());
        super::super::apply_subagent_thinking(&mut runtime, retained).unwrap();
        assert_eq!(runtime.reasoning_level().as_str(), "low");
        super::super::apply_subagent_thinking(&mut runtime, overridden).unwrap();
        assert_eq!(runtime.reasoning_level().as_str(), "xhigh");
    }

    #[test]
    fn subagent_thinking_legacy_archive_defaults_without_migration() {
        let meta: crate::runtime::subagent::SubagentArchiveMeta = serde_json::from_value(json!({
            "agent_name":"old", "model":"openai-codex/gpt-6-astra",
            "system_prompt":"test", "timeout_secs":300
        })).unwrap();
        assert!(meta.thinking.is_none());
        assert_eq!(super::super::resolve_subagent_thinking(&json!({}), meta.thinking.as_deref(), &Default::default()).unwrap().as_str(), "xhigh");
    }

    #[tokio::test]
    async fn reaped_worker_resumes_from_its_archive() {
        let layout = layout();
        let path = archive_in(&layout.dir, "sa_41");
        let registry = Mutex::new(SubagentRegistry::new());
        {
            let mut reg = registry.lock().unwrap();
            let mut h = finished_handle("sa_41", SubagentStatus::TimedOut, Some(path));
            h.mark_collected();
            reg.register(h);
            reg.cleanup_finished_with_ttl(Duration::ZERO);
            assert!(reg.get("sa_41").is_none(), "handle reaped");
        }
        let prior = prior_run(&registry, "sa_41", None, "render pass 3", &env(&layout))
            .await
            .unwrap();
        assert!(matches!(prior.retrieval, ResumeRetrieval::FullHistory));
        assert_eq!(prior.warning, None);
        // A v1 archive has no lineage: a successor starts a fresh one.
        assert_eq!(prior.lineage, None);
        assert_eq!(prior.agent_name, "archived-label");
        assert_eq!(prior.model, "anthropic/claude-opus-5-5");
        assert_eq!(prior.system_prompt, "archived system");
        assert_eq!(prior.timeout_secs, 3500);
        assert_continues_archive(&prior, "render pass 3");
    }

    #[tokio::test]
    async fn tombstoned_worker_with_archive_resumes() {
        let layout = layout();
        let path = archive_in(&layout.dir, "sa_42");
        let registry = Mutex::new(SubagentRegistry::new());
        {
            let mut reg = registry.lock().unwrap();
            reg.register(finished_handle(
                "sa_42",
                SubagentStatus::Failed("x".into()),
                Some(path),
            ));
            reg.release_finished_resources("sa_42");
            assert!(reg.get("sa_42").unwrap().is_tombstone());
        }
        let prior = prior_run(&registry, "sa_42", None, "continue", &env(&layout))
            .await
            .unwrap();
        // A live handle's identity wins over the archive's.
        assert_eq!(prior.agent_name, "live-label");
        assert_eq!(prior.timeout_secs, 30);
        assert_continues_archive(&prior, "continue");
    }

    #[tokio::test]
    async fn review_202_terminal_but_unpublished_worker_must_not_resume_lossily() {
        let layout = layout();
        let registry = Mutex::new(SubagentRegistry::new());
        let h = finished_handle("sa_43", SubagentStatus::TimedOut, None);
        {
            let mut s = h.state.write().unwrap();
            // Unlike the intentionally legacy helper, a managed worker has metadata.
            s.archive_meta = Some(crate::runtime::subagent::SubagentArchiveMeta {
                agent_name: "worker".into(),
                model: "model".into(),
                system_prompt: "system".into(),
                timeout_secs: 1,
                cwd: None,
                thinking: None,
            watch_usage: None,
            });
            s.finished_at = None;
        }
        let state = Arc::clone(&h.state);
        registry.lock().unwrap().register(h);
        assert!(
            prior_run(&registry, "sa_43", None, "continue", &env(&layout))
                .await
                .is_err()
        );
        registry
            .lock()
            .unwrap()
            .cleanup_finished_with_ttl(Duration::ZERO);
        assert!(registry.lock().unwrap().get("sa_43").is_some());
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, b"x").unwrap();
        let history = vec![
            Arc::new(json!({"role":"user", "content":"task"})),
            Arc::new(json!({"role":"assistant", "content":[
                {"type":"tool_use", "id":"t1", "name":"read", "input":{"path":"x"}}
            ]})),
            Arc::new(json!({"role":"user", "content":[
                {"type":"tool_result", "tool_use_id":"t1", "content":"preserved body"}
            ]})),
        ];
        state.write().unwrap().history = Some(history);
        super::super::finalize::finalize_subagent_in(
            &state,
            None,
            "sa_43",
            1,
            "worker",
            std::time::Instant::now(),
            None,
            &blocked,
        );
        // Keep this test hermetic: missing metadata prevents the production-dir retry.
        state.write().unwrap().archive_meta = None;
        let prior = prior_run(&registry, "sa_43", None, "continue", &env(&layout))
            .await
            .unwrap();
        assert_eq!(prior.messages[2]["content"][0]["content"], "preserved body");
        {
            let mut reg = registry.lock().unwrap();
            reg.get_mut("sa_43").unwrap().mark_collected();
            reg.release_finished_resources("sa_43");
            reg.cleanup_finished_with_ttl(Duration::ZERO);
            assert!(reg.get("sa_43").is_some());
        }
        assert!(state.read().unwrap().history.is_some());
    }

    /// A history well above a 1k-token threshold: bulky tool results.
    fn bulky_history() -> Vec<crate::SharedMessage> {
        let mut h = vec![Arc::new(json!({"role": "user", "content": "render the reel"}))];
        for i in 0..6 {
            h.push(Arc::new(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": format!("t{i}"), "name": "bash",
                 "input": {"command": format!("render {i}")}}
            ]})));
            h.push(Arc::new(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": format!("t{i}"), "content": "z".repeat(4000)}
            ]})));
        }
        h.push(Arc::new(json!({"role": "assistant", "content": "reel rendered"})));
        h
    }

    const COMPACT: ResumePolicy = ResumePolicy {
        full_history: false,
        compact_tokens: 1_000,
    };

    fn unix_ms(at: std::time::SystemTime) -> u64 {
        at.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
    }

    /// A v1 archive of [`bulky_history`] whose last request was at `anchor`.
    fn bulky_archive_in(dir: &std::path::Path, id: &str, anchor: u64) -> std::path::PathBuf {
        let meta = crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: "archived-label".into(),
            model: "anthropic/claude-opus-5-5".into(),
            system_prompt: "archived system".into(),
            timeout_secs: 3500,
            cwd: None,
            thinking: Some("max".into()),
            watch_usage: None,
        };
        super::super::archive::write_archive_anchored(
            dir,
            id,
            &meta,
            "completed",
            None,
            "reel rendered",
            &bulky_history(),
            Some(anchor),
        )
        .unwrap()
    }

    // Upstream's (#315), on ours' record locations.
    #[tokio::test]
    async fn cold_large_archive_resumes_compacted_and_full_history_overrides() {
        let layout = layout();
        let old_ms = unix_ms(std::time::SystemTime::now() - Duration::from_secs(3600));
        let path = bulky_archive_in(&layout.dir, "sa_76", old_ms);
        let history = bulky_history();
        let before = std::fs::read(&path).unwrap();
        let registry = Mutex::new(SubagentRegistry::new());
        let explicit = || Some(locate(&layout, &path));

        let prior = prior_run_as(&registry, "sa_76", explicit(), "add titles", &env(&layout), false, snapshot(COMPACT))
            .await
            .unwrap();
        let c = prior.compacted.expect("cold + big compacts");
        // Compacting the history must not drop the worker's recorded effort.
        assert_eq!(prior.thinking.as_deref(), Some("max"));
        assert_eq!(c.messages, history.len());
        assert!(c.cache_age_secs.unwrap() >= 3600);
        assert_eq!(prior.messages.len(), 1);
        let text = prior.messages[0]["content"].to_string();
        assert!(text.contains("render the reel") && text.contains("add titles"));

        // The prior archive is untouched, so a full replay is still possible.
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let full = ResumePolicy {
            full_history: true,
            ..COMPACT
        };
        let prior = prior_run_as(&registry, "sa_76", explicit(), "add titles", &env(&layout), false, snapshot(full))
            .await
            .unwrap();
        assert!(prior.compacted.is_none());
        assert_eq!(prior.thinking.as_deref(), Some("max"));
        assert_eq!(prior.messages.len(), history.len() + 1);

        // A warm archive (fresh anchor) replays in full.
        let warm = bulky_archive_in(&layout.dir, "sa_77", unix_ms(std::time::SystemTime::now()));
        let prior = prior_run_as(&registry, "sa_77", Some(locate(&layout, &warm)), "x", &env(&layout), false, snapshot(COMPACT))
            .await
            .unwrap();
        assert!(prior.compacted.is_none());
    }

    // Upstream's (#315).
    #[tokio::test]
    async fn live_unarchived_history_uses_state_anchor_for_the_cold_check() {
        let layout = layout();
        let registry = Mutex::new(SubagentRegistry::new());
        let h = finished_handle("sa_78", SubagentStatus::Completed, None);
        {
            let mut s = h.state.write().unwrap();
            s.archive_meta = Some(crate::runtime::subagent::SubagentArchiveMeta {
                agent_name: "worker".into(),
                model: "model".into(),
                system_prompt: "system".into(),
                timeout_secs: 1,
                cwd: None,
                thinking: Some("medium".into()),
                watch_usage: None,
            });
            s.archive_error = Some("archive write failed".into());
            s.finalization_ready = true;
            s.history = Some(bulky_history());
            // Finished just now, but its last request was an hour ago.
            s.cache_anchor_unix_ms =
                Some(unix_ms(std::time::SystemTime::now() - Duration::from_secs(3600)));
        }
        let state = Arc::clone(&h.state);
        registry.lock().unwrap().register(h);
        let prior = prior_run_as(&registry, "sa_78", None, "go", &env(&layout), false, snapshot(COMPACT))
            .await
            .unwrap();
        assert!(prior.compacted.is_some());
        assert_eq!(prior.thinking.as_deref(), Some("medium"));
        // Without an anchor, `finished_at` (just now) says warm.
        state.write().unwrap().cache_anchor_unix_ms = None;
        let prior = prior_run_as(&registry, "sa_78", None, "go", &env(&layout), false, snapshot(COMPACT))
            .await
            .unwrap();
        assert!(prior.compacted.is_none());
        assert_eq!(prior.thinking.as_deref(), Some("medium"));
    }

    /// E9: the daemon's reload resume never compacts (it keeps its own
    /// words and continues at once), nor does a policy with compaction off,
    /// as `launch` sets it for a worker that inherits the session's context
    /// management (mutant: compact in the daemon arm, or ignore
    /// `compact_tokens == 0`).
    #[tokio::test]
    async fn e9_cold_resume_compaction_yields_to_the_daemon_and_to_inherited_context() {
        let layout = layout();
        let old_ms = unix_ms(std::time::SystemTime::now() - Duration::from_secs(3600));
        let path = bulky_archive_in(&layout.dir, "sa_79", old_ms);
        let registry = Mutex::new(SubagentRegistry::new());
        let words = super::super::finalize::DAEMON_RESUME_INSTRUCTIONS;
        let prior = prior_run_as(&registry, "sa_79", Some(locate(&layout, &path)), words, &env(&layout), true, snapshot(COMPACT))
            .await
            .unwrap();
        assert!(prior.compacted.is_none());
        assert_eq!(prior.messages.len(), bulky_history().len() + 1);
        let inherited = ResumePolicy {
            compact_tokens: 0,
            ..COMPACT
        };
        let prior = prior_run_as(&registry, "sa_79", Some(locate(&layout, &path)), "go", &env(&layout), false, snapshot(inherited))
            .await
            .unwrap();
        assert!(prior.compacted.is_none());
        assert_eq!(prior.messages.len(), bulky_history().len() + 1);
    }

    #[tokio::test]
    async fn running_worker_cannot_be_resumed() {
        let layout = layout();
        let registry = Mutex::new(SubagentRegistry::new());
        registry
            .lock()
            .unwrap()
            .register(finished_handle("sa_run", SubagentStatus::Running, None));
        let err = prior_run(&registry, "sa_run", None, "x", &env(&layout))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("still running"), "{err}");
    }

    #[tokio::test]
    async fn explicit_archive_ignores_the_registry() {
        // An archive from an earlier session can share a handle id with a
        // worker running now; the registry must not be consulted.
        let layout = layout();
        let path = archive_in(&layout.dir, "sa_3");
        let registry = Mutex::new(SubagentRegistry::new());
        registry
            .lock()
            .unwrap()
            .register(finished_handle("sa_3", SubagentStatus::Running, None));
        let prior = prior_run(
            &registry,
            "sa_3",
            Some(locate(&layout, &path)),
            "pick up",
            &env(&layout),
        )
        .await
        .unwrap();
        assert_eq!(prior.agent_name, "archived-label");
        assert_continues_archive(&prior, "pick up");
    }

    #[tokio::test]
    async fn a_v1_archive_over_the_record_cap_still_resumes() {
        // The v1 writer never had a cap and real archives exceed it (strict
        // r1 A-B1(a)): a v1 archive is read whole, as before the port, while
        // a v2 record keeps the cap. The cap is injected small, so nothing
        // near 64 MiB is written.
        let layout = layout();
        let v1 = archive_in(&layout.dir, "sa_21");
        let store = fx::legacy();
        let rec = fx::record(&fx::run(9), fx::current(&store.binding), vec![], &[], None);
        let v2 = write_record(&layout.dir, "sa_5.json", &rec);
        let size = |p: &std::path::Path| std::fs::metadata(p).unwrap().len();
        let limit = size(&v1).min(size(&v2)) / 2;
        let _seam = record_file::seams::hook(|_| {});
        record_file::seams::limit_records(limit);
        let registry = Mutex::new(SubagentRegistry::new());
        let prior = prior_run(
            &registry,
            "sa_21",
            Some(locate(&layout, &v1)),
            "go on",
            &env(&layout),
        )
        .await
        .unwrap();
        assert!(matches!(prior.retrieval, ResumeRetrieval::FullHistory));
        assert_continues_archive(&prior, "go on");
        let err = match prior_run(
            &registry,
            "sa_5",
            Some(locate(&layout, &v2)),
            "go on",
            &env(&layout),
        )
        .await
        {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a v2 record over the cap resumed"),
        };
        assert!(
            err.contains(&format!("is {} bytes, over the", size(&v2))),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_v1_archive_in_a_legacy_process_dir_resumes() {
        // An earlier writer named its process directory
        // YYYYMMDD-HHMMSS-<pid>. Real history still holds v1 archives there
        // and the live reader resumes them, so the port must too.
        let layout = layout();
        let path = archive_in(&layout.root.join("20261008-000144-2531188"), "sa_76");
        let registry = Mutex::new(SubagentRegistry::new());
        let prior = prior_run(
            &registry,
            "sa_76",
            Some(locate(&layout, &path)),
            "go on",
            &env(&layout),
        )
        .await
        .unwrap();
        assert!(matches!(prior.retrieval, ResumeRetrieval::FullHistory));
        assert_eq!(prior.agent_name, "archived-label");
        assert_continues_archive(&prior, "go on");
    }

    #[tokio::test]
    async fn an_explicit_paths_writer_probe_runs_before_the_registry_guard() {
        // Strict r1 A-LOW: the run-lock probe is filesystem work, so for an
        // explicit archive_path it runs with the registry guard free.
        let layout = layout();
        let path = archive_in(&layout.dir, "sa_21");
        // A free run-lock file, so the probe takes (and drops) its shared lock.
        fx::private_file(
            &layout
                .dir
                .join(super::super::record_store::lock_file_name("sa_21")),
            b"",
        );
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        let free = std::rc::Rc::new(std::cell::Cell::new(None));
        let (seen, reg) = (free.clone(), registry.clone());
        super::super::record_store::seams::on_probe_shared(Some(Box::new(move || {
            seen.set(Some(reg.try_lock().is_ok()));
        })));
        let prior = prior_run(
            &registry,
            "sa_21",
            Some(locate(&layout, &path)),
            "go on",
            &env(&layout),
        )
        .await;
        super::super::record_store::seams::on_probe_shared(None);
        assert_continues_archive(&prior.unwrap(), "go on");
        assert_eq!(free.get(), Some(true), "probed with the registry guard free");
    }

    #[tokio::test]
    async fn empty_history_archive_is_refused_not_resumed_blank() {
        // A startup-failure archive (worker exited before its first
        // checkpoint) is written with no history; resuming it would start
        // the worker with nothing but a preamble claiming earlier work.
        let layout = layout();
        let meta = crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: "archived-label".into(),
            model: "anthropic/claude-opus-5-5".into(),
            system_prompt: "archived system".into(),
            timeout_secs: 30,
            cwd: None,
            thinking: None,
            watch_usage: None,
        };
        // Named as the writer names it: admission locates an archive by
        // the store's own path forms (the port's strict paths).
        let path = super::super::archive::write_archive(
            &layout.dir,
            "sa_62",
            &meta,
            "failed",
            Some("provider refused the model"),
            "",
            &[],
        )
        .unwrap();
        let registry = Mutex::new(SubagentRegistry::new());
        let err = match prior_run(
            &registry,
            "sa_62",
            Some(locate(&layout, &path)),
            "go on",
            &env(&layout),
        )
        .await
        {
            Err(e) => e.to_string(),
            Ok(_) => panic!("empty archive resumed"),
        };
        assert!(err.contains("no conversation to resume"), "{err}");
        assert!(err.contains("provider refused the model"), "{err}");
        // Same through the registry (reaped handle, archive on record).
        {
            let mut reg = registry.lock().unwrap();
            let mut h = finished_handle("sa_62", SubagentStatus::Failed("x".into()), Some(path));
            h.mark_collected();
            reg.register(h);
            reg.cleanup_finished_with_ttl(Duration::ZERO);
        }
        let err = match prior_run(&registry, "sa_62", None, "go on", &env(&layout)).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("empty archive resumed through the registry"),
        };
        assert!(err.contains("no conversation to resume"), "{err}");
    }

    #[tokio::test]
    async fn legacy_handle_without_archive_pastes_prior_text() {
        let layout = layout();
        let registry = Mutex::new(SubagentRegistry::new());
        registry.lock().unwrap().register(finished_handle(
            "sa_old",
            SubagentStatus::Completed,
            None,
        ));
        let prior = prior_run(&registry, "sa_old", None, "next", &env(&layout))
            .await
            .unwrap();
        assert_eq!(prior.messages.len(), 1);
        assert_eq!(prior.lineage, None);
        let text = prior.messages[0]["content"].as_str().unwrap();
        assert!(text.starts_with("next") && text.contains("prior text"));
    }

    #[tokio::test]
    async fn unknown_handle_without_archive_is_not_found() {
        let layout = layout();
        let registry = Mutex::new(SubagentRegistry::new());
        let err = prior_run(&registry, "sa_none", None, "x", &env(&layout))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("No subagent found"), "{err}");
    }

    #[tokio::test]
    async fn tombstone_resume_reports_expired_context_without_side_effects() {
        let state = Arc::new(RwLock::new(SubagentState::new()));
        {
            let mut state = state.write().unwrap();
            state.status = SubagentStatus::Completed;
            state.partial_text = "terminal output".into();
            state.conversation_state = vec![json!({"role": "assistant", "content": "context"})];
            state.finished_at = Some(std::time::Instant::now());
        }
        let (steer_tx, _steer_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = oneshot::channel();
        let (_result_tx, result_rx) = oneshot::channel();
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        registry.lock().unwrap().register(SubagentHandle::new(
            "sa_expired".into(),
            1,
            "test".into(),
            "task".into(),
            "anthropic/claude-sonnet-4-6".into(),
            "system".into(),
            30,
            state,
            Some(steer_tx),
            Some(shutdown_tx),
            Some(result_rx),
        ));
        let foreground =
            agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap();
        let orchestration = Arc::new(
            crate::orchestration::OrchestrationRuntime::baseline(foreground, 8, 64).unwrap(),
        );
        orchestration
            .authorize("sa_expired", "anthropic/claude-sonnet-4-6")
            .unwrap();
        let gate_before = orchestration.completion_gate();
        reap_finished_with_ttl(&registry, Some(orchestration.as_ref()), Duration::ZERO);
        assert!(registry
            .lock()
            .unwrap()
            .get("sa_expired")
            .unwrap()
            .is_tombstone());
        let count_before = registry.lock().unwrap().list_active().len();

        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(registry.clone());
        ctx.capabilities.orchestration = Some(orchestration.clone());
        let error = SubagentResumeTool
            .execute(
                json!({"handle_id": "sa_expired", "instructions": "continue"}),
                ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("expired resumable context"));
        assert!(!error.contains("No subagent found"));
        assert_eq!(registry.lock().unwrap().list_active().len(), count_before);
        assert_eq!(orchestration.completion_gate(), gate_before);
    }

    #[tokio::test]
    async fn a_spent_watch_lease_refuses_the_resume_and_releases_what_it_reserved() {
        let layout = layout();
        let meta = crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: "t-run-wait".into(),
            model: "anthropic/claude-sonnet-4-6".into(),
            system_prompt: "watch".into(),
            timeout_secs: 300,
            cwd: None,
            thinking: None,
            watch_usage: Some(crate::runtime::subagent::WatchUsage {
                secs: 7200,
                rounds: 3,
            }),
        };
        let history: Vec<crate::SharedMessage> = vec![
            Arc::new(json!({"role": "user", "content": "wait for the run"})),
            Arc::new(json!({"role": "assistant", "content": "still running"})),
        ];
        // Named as the writer names it (the port's strict paths).
        let path = super::super::archive::write_archive(
            &layout.dir,
            "sa_61",
            &meta,
            "timed_out",
            Some("watch lease expired"),
            "report",
            &history,
        )
        .unwrap();
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        {
            let mut reg = registry.lock().unwrap();
            let mut h = finished_handle("sa_61", SubagentStatus::TimedOut, Some(path));
            h.mark_collected();
            reg.register(h);
            reg.cleanup_finished_with_ttl(Duration::ZERO);
        }
        let foreground =
            agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap();
        let orchestration = Arc::new(
            crate::orchestration::OrchestrationRuntime::baseline(foreground, 8, 64).unwrap(),
        );
        // A worker dispatched earlier, and a parent the refused resume would
        // have nested under: the refusal must leave both exactly as they are.
        orchestration
            .resolve_and_authorize("sa_earlier", None)
            .expect("an earlier dispatch");
        orchestration
            .reserve_delegation("sa_parent", None)
            .expect("a parent edge");
        let handles_before = orchestration.unreconciled_runtime_handles();
        assert!(handles_before.contains("sa_earlier"), "{handles_before:?}");
        let descendants_before = orchestration.delegation_descendants();
        let gate_before = orchestration.completion_gate();
        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(registry.clone());
        ctx.capabilities.orchestration = Some(orchestration.clone());
        ctx.capabilities.delegation_parent = Some("sa_parent".into());
        let text = SubagentResumeTool
            .execute_in(
                json!({"handle_id": "sa_61", "instructions": "check the run again"}),
                ctx,
                &env(&layout),
            )
            .await
            .expect("a refusal is a tool result");
        assert!(
            text.starts_with("subagent_resume refused (watch lease expired"),
            "{text}"
        );
        assert!(text.contains("7200s of 7200s"), "{text}");
        assert!(registry.lock().unwrap().list_active().is_empty());
        // The dispatch it authorized is rolled back (a Dispatched worker is
        // unreconciled until then), and its tree edge is released.
        assert_eq!(
            orchestration.unreconciled_runtime_handles(),
            handles_before,
            "the refused dispatch was rolled back"
        );
        assert_eq!(orchestration.delegation_descendants(), descendants_before);
        assert_eq!(orchestration.completion_gate(), gate_before);
    }

    /// S0 (scaffolding for slice 9): a test site gives a resume a plan
    /// whatever the run wants, as the oneshot's does; without the parent's
    /// settings it falls back to rollover off and says why. A resume
    /// without one gets none while worker rollover is off. A planned resume
    /// of a legacy prior (no owned grant) asks for the binding its records
    /// go on, as production's does (the configured one); an unplanned one
    /// never asks (mutants: the launch ignores `ResumeEnv::planned`; the
    /// production site is taken unasked).
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn s0_a_test_site_gives_a_resume_a_plan() {
        // A real worker thread starts: an empty base directory and no
        // provider credentials, so it fails at auth, offline.
        let _base = crate::test_env::BaseDirGuard::new();
        let _offline = crate::test_env::OfflineProviderEnv::new();
        let layout = layout();
        let store = fx::legacy();
        let planned_binding = binding_of(&store.binding);
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        let mut notices = Vec::new();
        for (prior, planned) in [("sa_90", false), ("sa_91", true)] {
            registry
                .lock()
                .unwrap()
                .register(finished_handle(prior, SubagentStatus::Completed, None));
            let mut ctx = create_tool_context();
            ctx.capabilities.subagent_registry = Some(registry.clone());
            assert!(ctx.capabilities.worker_context.is_none());
            let binding: &(dyn Fn(&crate::config::SynapsConfig, Option<&std::path::Path>) -> MemoryBinding
                  + Send
                  + Sync) = if planned {
                &planned_binding
            } else {
                &no_binding
            };
            let site_env = ResumeEnv {
                binding,
                planned: planned.then(|| {
                    super::super::oneshot::RecordSite::at(
                        layout.root.clone(),
                        OTHER_PROCESS_DIR.to_owned(),
                    )
                }),
                ..env(&layout)
            };
            let reply = SubagentResumeTool
                .execute_in(
                    json!({"handle_id": prior, "instructions": "go on", "timeout": 1}),
                    ctx,
                    &site_env,
                )
                .await
                .expect("resumed");
            let body: Value = serde_json::from_str(&reply).unwrap();
            let handle_id = body["handle_id"].as_str().unwrap().to_owned();
            let thread = {
                let mut reg = registry.lock().unwrap();
                let handle = reg.get_mut(&handle_id).expect("registered");
                handle.cancel();
                handle
                    .thread_handle
                    .take()
                    .expect("the thread handle is attached before dispatch returns")
            };
            let joined = tokio::time::timeout(
                Duration::from_secs(60),
                tokio::task::spawn_blocking(move || thread.join().is_ok()),
            )
            .await
            .expect("a cancelled worker stops within its 1 s timeout")
            .unwrap();
            assert!(joined, "the resumed worker ends cleanly");
            let reg = registry.lock().unwrap();
            let handle = reg.get(&handle_id).expect("registered");
            let notice = handle.state.read().unwrap().rollover_notice.clone();
            notices.push(notice);
        }
        assert_eq!(
            notices,
            [
                None,
                Some(
                    "rollover disabled for this worker: the parent's context settings are not available"
                        .to_owned()
                ),
            ]
        );
    }

    // ── Slice 4: resume admission wiring (dark) ─────────────────────────────

    use super::super::record::{ArchiveId, RecordKind, WorkerRecord};
    use super::super::test_fixtures as fx;

    /// A record file at `<dir>/<name>`, private like the writer's.
    fn write_record(dir: &std::path::Path, name: &str, rec: &WorkerRecord) -> std::path::PathBuf {
        fx::private_dir(dir.parent().unwrap());
        fx::private_dir(dir);
        let path = dir.join(name);
        fx::private_file(&path, &serde_json::to_vec(rec).unwrap());
        path
    }

    fn binding_of(
        binding: &MemoryBinding,
    ) -> impl Fn(&crate::config::SynapsConfig, Option<&std::path::Path>) -> MemoryBinding + Send + Sync
    {
        let binding = binding.clone();
        move |_, _| binding.clone()
    }

    fn last_text(prior: &PriorRun) -> String {
        let last = prior.messages.last().unwrap();
        let blocks = last["content"].as_array().unwrap();
        blocks.last().unwrap()["text"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn w1_shortened_v2_record_resumes_with_owned_history_in_the_worker_registry() {
        let store = fx::legacy();
        let r = fx::run(7);
        let w1 = fx::seal_run(&store.binding, &r, "NEEDLE first window").await;
        let w2 = fx::seal_run(&store.binding, &r, "NEEDLE second window").await;
        let sibling = fx::seal_run(&store.binding, &fx::run(8), "NEEDLE sibling").await;
        let rec = fx::record(
            &r,
            fx::current(&store.binding),
            vec![],
            &[w1.clone(), w2.clone()],
            None,
        );
        let layout = layout();
        let path = write_record(&layout.dir, "sa_5.json", &rec);
        let binding = binding_of(&store.binding);
        let env = ResumeEnv {
            root: &layout.root,
            own_process_dir: OTHER_PROCESS_DIR,
            binding: &binding,
            worker_tools: None,
            planned: None,
        };
        let registry = Mutex::new(SubagentRegistry::new());
        let prior = prior_run(
            &registry,
            "sa_1",
            Some(locate(&layout, &path)),
            "go on",
            &env,
        )
        .await
        .unwrap();
        assert_eq!(
            (prior.agent_name.as_str(), prior.model.as_str()),
            ("worker", "provider/model-b")
        );
        assert_eq!(prior.timeout_secs, 300);
        assert_eq!(prior.warning, None);
        // A rollover-enabled successor continues the record's lineage.
        assert_eq!(
            prior.lineage,
            Some(super::super::lineage::successor_lineage(&rec, RecordKind::Final).unwrap())
        );
        // The record's current window, verbatim, then the instructions.
        assert_eq!(prior.messages[0]["_synaps_context"]["archive"], w2.as_str());
        assert!(last_text(&prior).ends_with("go on"));
        let ResumeRetrieval::Owned { grant, evicted } = &prior.retrieval else {
            panic!("a shortened history needs owned retrieval");
        };
        assert!(evicted.is_empty());
        let mut expected = vec![w1.as_str().to_owned(), w2.as_str().to_owned()];
        expected.sort();
        assert_eq!(grant.allow(), expected.as_slice());

        // In the resumed worker's registry, memory_search is the owned tool.
        let mut tools = crate::ToolRegistry::without_subagent();
        super::super::retrieval::install_owned_history(&mut tools, Arc::clone(grant)).unwrap();
        let search = Arc::clone(tools.get("memory_search").unwrap());
        let out = search
            .execute(
                json!({"source": "history", "query": "NEEDLE"}),
                create_tool_context(),
            )
            .await
            .unwrap();
        assert!(
            out.contains(&fx::ctx(&w1)) && out.contains(&fx::ctx(&w2)),
            "{out}"
        );
        assert!(!out.contains(&fx::ctx(&sibling)), "{out}");
        let notes = search
            .execute(json!({"query": "NEEDLE"}), create_tool_context())
            .await
            .unwrap_err()
            .to_string();
        assert!(notes.contains("source=history is required"), "{notes}");
    }

    #[tokio::test]
    async fn w2_head_only_resumes_as_a_crash_checkpoint_with_a_warning() {
        let store = fx::legacy();
        let r = fx::run(2);
        let w1 = fx::seal_run(&store.binding, &r, "first").await;
        let mut head = fx::record(&r, fx::current(&store.binding), vec![], &[w1], None);
        head.kind = RecordKind::Head;
        let layout = layout();
        write_record(&layout.dir, "sa_5.head.json", &head);
        let binding = binding_of(&store.binding);
        let env = ResumeEnv {
            root: &layout.root,
            own_process_dir: OTHER_PROCESS_DIR,
            binding: &binding,
            worker_tools: None,
            planned: None,
        };
        let registry = Mutex::new(SubagentRegistry::new());
        // Named by its final's path: the head is found as its sibling.
        let prior = prior_run(
            &registry,
            "sa_1",
            Some(locate(&layout, &layout.dir.join("sa_5.json"))),
            "go on",
            &env,
        )
        .await
        .unwrap();
        assert_eq!(
            prior.warning,
            Some(super::super::admission::CRASH_CHECKPOINT_WARNING)
        );
        assert!(matches!(prior.retrieval, ResumeRetrieval::Owned { .. }));
        assert_eq!(
            prior.lineage,
            Some(super::super::lineage::successor_lineage(&head, RecordKind::Head).unwrap())
        );
    }

    /// Train30 item 4b: a reload-stopped run's head left without a final is
    /// marked cancelled in place, atomically (a temp file in its directory
    /// renamed over it): only its status changes, so it still resumes by
    /// its path as a crash checkpoint, with the warning. Marking again
    /// changes nothing; a head whose final is published is never touched.
    #[tokio::test]
    // Unix only: sets file permissions (parity-platform).
    #[cfg(unix)]
    async fn w2b_a_head_marked_cancelled_still_resumes_as_a_crash_checkpoint() {
        use super::super::crash_head::{mark_cancelled, Marked};
        let store = fx::legacy();
        let r = fx::run(2);
        let w1 = fx::seal_run(&store.binding, &r, "first").await;
        let mut head = fx::record(&r, fx::current(&store.binding), vec![], &[w1], None);
        head.kind = RecordKind::Head;
        head.status = "running".into();
        let layout = layout();
        let path = write_record(&layout.dir, "sa_5.head.json", &head);

        assert_eq!(mark_cancelled(&path, &layout.root), Ok(Marked::Rewritten));
        let marked: WorkerRecord = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(marked.status, "cancelled");
        assert_eq!(
            WorkerRecord {
                status: head.status.clone(),
                ..marked.clone()
            },
            head,
            "only its status changed"
        );
        let names: Vec<String> = std::fs::read_dir(&layout.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["sa_5.head.json"], "no temp file left");
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "still private");
        }
        let bytes = std::fs::read(&path).unwrap();
        assert!(matches!(
            mark_cancelled(&path, &layout.root),
            Ok(Marked::Unchanged(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "marked once");

        let binding = binding_of(&store.binding);
        let env = ResumeEnv {
            root: &layout.root,
            own_process_dir: OTHER_PROCESS_DIR,
            binding: &binding,
            worker_tools: None,
            planned: None,
        };
        let registry = Mutex::new(SubagentRegistry::new());
        // By the head's own path, as the parent's wake gives it.
        let prior = prior_run(&registry, "sa_1", Some(locate(&layout, &path)), "go on", &env)
            .await
            .unwrap();
        assert_eq!(
            prior.warning,
            Some(super::super::admission::CRASH_CHECKPOINT_WARNING)
        );
        assert_eq!(
            prior.lineage,
            Some(super::super::lineage::successor_lineage(&head, RecordKind::Head).unwrap())
        );

        // A head beside its published final: the final is the run's record.
        let other = self::layout();
        let head_path = write_record(&other.dir, "sa_5.head.json", &head);
        let mut fin = head.clone();
        fin.kind = RecordKind::Final;
        write_record(&other.dir, "sa_5.json", &fin);
        let before = std::fs::read(&head_path).unwrap();
        assert!(matches!(
            mark_cancelled(&head_path, &other.root),
            Ok(Marked::Unchanged(_))
        ));
        assert_eq!(std::fs::read(&head_path).unwrap(), before);
    }

    #[tokio::test]
    async fn w3_unshortened_v2_record_is_still_checked_against_the_binding() {
        let store = fx::legacy();
        let rec = fx::record(&fx::run(3), fx::current(&store.binding), vec![], &[], None);
        let layout = layout();
        let path = write_record(&layout.dir, "sa_5.json", &rec);
        let registry = Mutex::new(SubagentRegistry::new());
        let resume = |binding: MemoryBinding| {
            let layout = &layout;
            let path = path.clone();
            let registry = &registry;
            async move {
                let binding = binding_of(&binding);
                let env = ResumeEnv {
                    root: &layout.root,
                    own_process_dir: OTHER_PROCESS_DIR,
                    binding: &binding,
                    worker_tools: None,
                    planned: None,
                };
                prior_run(registry, "sa_1", Some(locate(layout, &path)), "x", &env).await
            }
        };
        let prior = resume(store.binding.clone()).await.unwrap();
        assert!(matches!(prior.retrieval, ResumeRetrieval::FullHistory));
        assert_eq!(
            prior.lineage,
            Some(super::super::lineage::successor_lineage(&rec, RecordKind::Final).unwrap())
        );
        let other = fx::legacy_binding(&store.base, "project-b");
        let err = resume(other).await.unwrap_err().to_string();
        assert!(
            err.contains("another memory backend or repository"),
            "{err}"
        );
    }

    #[tokio::test]
    // Unix only: plants a symlink (parity-platform).
    #[cfg(unix)]
    async fn w4_refusals_surface_their_reason_before_anything_is_reserved() {
        let store = fx::legacy();
        let r = fx::run(4);
        let foreign = fx::seal_run(&store.binding, &fx::run(9), "foreign").await;
        let own = fx::seal_run(&store.binding, &r, "own").await;
        let layout = layout();
        let current = fx::current(&store.binding);
        // A window sealed by another run, listed as this run's own.
        let mut rec = fx::record(&r, current.clone(), vec![], &[foreign], None);
        rec.handle_id = "sa_11".into();
        write_record(&layout.dir, "sa_11.json", &rec);
        // A head newer than its final.
        let mut fin = fx::record(
            &r,
            current.clone(),
            vec![],
            std::slice::from_ref(&own),
            None,
        );
        fin.handle_id = "sa_12".into();
        let mut head = fin.clone();
        head.kind = RecordKind::Head;
        head.generation = fin.generation + 1;
        write_record(&layout.dir, "sa_12.json", &fin);
        write_record(&layout.dir, "sa_12.head.json", &head);
        // A record reached through an in-root symlink.
        std::os::unix::fs::symlink(layout.dir.join("sa_12.json"), layout.dir.join("sa_13.json"))
            .unwrap();
        // A legacy archive whose history was shortened.
        let shortened: Vec<crate::SharedMessage> = vec![
            Arc::new(fx::marker_message(&own, 2)),
            Arc::new(json!({"role": "assistant", "content": "working"})),
        ];
        super::super::archive::write_archive(
            &layout.dir,
            "sa_14",
            &fx::meta().into(),
            "completed",
            None,
            "",
            &shortened,
        )
        .unwrap();
        let cases = [
            (layout.dir.join("sa_11.json"), "belongs to another run"),
            (
                layout.dir.join("sa_12.json"),
                "is newer than the final record",
            ),
            (layout.dir.join("sa_13.json"), "crosses a symlink"),
            (
                layout.dir.join("sa_14.json"),
                "legacy worker archive holds a shortened history",
            ),
            (layout.dir.join("sa_15.json"), "no worker record exists"),
            (layout.dir.join("notes.json"), "names no worker record file"),
            ("/etc/passwd".into(), "under the archive root"),
        ];
        let binding = binding_of(&store.binding);
        let env = ResumeEnv {
            root: &layout.root,
            own_process_dir: OTHER_PROCESS_DIR,
            binding: &binding,
            worker_tools: None,
            planned: None,
        };
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        for (path, reason) in cases {
            let mut ctx = create_tool_context();
            ctx.capabilities.subagent_registry = Some(Arc::clone(&registry));
            ctx.capabilities.memory_backend = Some(store.binding.clone());
            let orchestration = Arc::clone(ctx.capabilities.orchestration.as_ref().unwrap());
            let gate = orchestration.completion_gate();
            let err = SubagentResumeTool
                .execute_in(
                    json!({"handle_id": "sa_1", "instructions": "x",
                       "archive_path": path.to_str().unwrap()}),
                    ctx,
                    &env,
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(reason), "{reason}: {err}");
            assert!(err.contains("Nothing was started"), "{err}");
            assert!(registry.lock().unwrap().list_active().is_empty());
            assert_eq!(orchestration.delegation_descendants(), 0);
            assert_eq!(orchestration.completion_gate(), gate);
        }
    }

    #[tokio::test]
    async fn w5_a_worker_of_this_registry_that_may_still_write_is_not_resumed() {
        let store = fx::legacy();
        let rec = fx::record(&fx::run(5), fx::current(&store.binding), vec![], &[], None);
        let layout = layout();
        let path = write_record(&layout.dir, "sa_5.json", &rec);
        let binding = binding_of(&store.binding);
        let env = ResumeEnv {
            root: &layout.root,
            own_process_dir: fx::PROCESS_DIR,
            binding: &binding,
            worker_tools: None,
            planned: None,
        };
        let registry = Mutex::new(SubagentRegistry::new());
        let handle = finished_handle("sa_5", SubagentStatus::Running, None);
        let state = Arc::clone(&handle.state);
        registry.lock().unwrap().register(handle);
        let resume = || prior_run(&registry, "sa_1", Some(locate(&layout, &path)), "x", &env);
        let err = resume().await.unwrap_err().to_string();
        assert!(err.contains("may still be writing"), "{err}");
        // Terminal, but its final record is not published yet.
        state.write().unwrap().status = SubagentStatus::Completed;
        let err = resume().await.unwrap_err().to_string();
        assert!(err.contains("may still be writing"), "{err}");
        // Published: nothing of this run will be written again.
        state.write().unwrap().archive_path = Some(path.clone());
        assert!(resume().await.is_ok());
    }

    #[tokio::test]
    async fn w6_a_shortened_history_kept_in_memory_is_refused() {
        let layout = layout();
        let registry = Mutex::new(SubagentRegistry::new());
        let h = finished_handle("sa_44", SubagentStatus::TimedOut, None);
        {
            let mut s = h.state.write().unwrap();
            s.archive_error = Some("archive write failed".into());
            s.finalization_ready = true;
            let archive = ArchiveId::parse(&"ab".repeat(16)).unwrap();
            s.history = Some(vec![
                Arc::new(fx::marker_message(&archive, 2)),
                Arc::new(json!({"role": "assistant", "content": "working"})),
            ]);
        }
        registry.lock().unwrap().register(h);
        let err = prior_run(&registry, "sa_44", None, "x", &env(&layout))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("shortened history"), "{err}");
    }

    #[tokio::test]
    // Unix only: plants a symlink (parity-platform).
    #[cfg(unix)]
    async fn w7_this_process_archive_goes_through_the_same_record_checks() {
        let layout = layout();
        let real = archive_in(&layout.dir, "sa_45");
        let link = layout.dir.join("sa_46.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let registry = Mutex::new(SubagentRegistry::new());
        registry.lock().unwrap().register(finished_handle(
            "sa_46",
            SubagentStatus::Completed,
            Some(link),
        ));
        let err = prior_run(&registry, "sa_46", None, "x", &env(&layout))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("crosses a symlink"), "{err}");
    }

    /// A window ID of the fake Axel store, by byte.
    fn window(n: u8) -> ArchiveId {
        ArchiveId::parse(&format!("{n:02x}").repeat(16)).unwrap()
    }

    /// A fake Axel store speaking `history_owned` version 2: it holds
    /// `windows` (live, each under its run's logical ID) and has evicted
    /// `evictions` windows so far. Any other listed window is absent.
    // Unix only: `fx::FakeAxel` is (parity-platform).
    #[cfg(unix)]
    fn axel_v2(windows: &[(&ArchiveId, &super::super::record::RunId)], evictions: u64) -> fx::FakeAxel {
        let entries: Vec<Value> = windows
            .iter()
            .map(|(id, run)| {
                let logical = super::super::lineage::worker_logical_id(run);
                json!({
                    "id": id.as_str(),
                    "logical_id": agent_core::context_archive::logical_digest(&logical).unwrap(),
                    "live": true,
                })
            })
            .collect();
        fx::FakeAxel::new(json!({
            "capabilities": fx::owned_capabilities_v2(),
            "entries": entries,
            "rows": [],
            "fetch": {},
            "evictions": evictions,
        }))
    }

    /// B31: listed windows a store that has evicted history (its count above
    /// 0) no longer has do not refuse a resume. The worker is told once, in
    /// its resume turn; its successor carries the same lineage, so the
    /// successor's own resume is admitted again (evicting the same window
    /// again).
    #[tokio::test]
    // Unix only: runs on `axel_v2` (parity-platform).
    #[cfg(unix)]
    async fn b31_evicted_windows_resume_with_one_notice_and_again_from_the_successor() {
        let r = fx::run(7);
        // The store answers without it, as for a window it evicted.
        let evicted = window(0x5a);
        let (w2, w3) = (window(0x72), window(0x73));
        // The successor's own window (run 8), sealed after its rollover.
        let s = window(0x81);
        let fake = axel_v2(&[(&w2, &r), (&w3, &r), (&s, &fx::run(8))], 3);
        let store = &fake;
        let rec = fx::record(
            &r,
            fx::current(&store.binding),
            vec![],
            &[evicted.clone(), w2.clone(), w3.clone()],
            None,
        );
        let notice = super::super::lineage::evicted_notice(1).unwrap();
        let notices = |prior: &PriorRun| {
            prior
                .messages
                .iter()
                .map(|m| m.to_string().matches("no longer in memory").count())
                .sum::<usize>()
        };
        let binding = binding_of(&store.binding);
        let registry = Mutex::new(SubagentRegistry::new());

        let first = layout();
        let path = write_record(&first.dir, "sa_5.json", &rec);
        let env = ResumeEnv {
            root: &first.root,
            own_process_dir: OTHER_PROCESS_DIR,
            binding: &binding,
            worker_tools: None,
            planned: None,
        };
        let prior = prior_run(&registry, "sa_1", Some(locate(&first, &path)), "go on", &env)
            .await
            .expect("an evicted window does not refuse the resume");
        let ResumeRetrieval::Owned { grant, evicted: gone } = &prior.retrieval else {
            panic!("a shortened history keeps owned retrieval");
        };
        assert_eq!(gone, std::slice::from_ref(&evicted));
        let mut expected = vec![w2.as_str().to_owned(), w3.as_str().to_owned()];
        expected.sort();
        assert_eq!(grant.allow(), expected.as_slice());
        // One notice, between the resume note and the instructions.
        assert_eq!(notices(&prior), 1);
        let text = last_text(&prior);
        assert!(text.starts_with("[Resumed by the orchestrator."), "{text}");
        assert!(text.ends_with(&format!("{notice}\n\ngo on")), "{text}");
        // The successor carries the whole lineage, the evicted window too.
        let lineage = prior.lineage.clone().expect("a v2 record's lineage");
        assert_eq!(
            lineage,
            super::super::lineage::successor_lineage(&rec, RecordKind::Final).unwrap()
        );
        assert_eq!(lineage.inherited_windows[0].archive_id, evicted);

        // The successor rolled over once, then the daemon resumes it.
        let next = fx::record(
            &fx::run(8),
            fx::current(&store.binding),
            lineage.inherited_windows,
            std::slice::from_ref(&s),
            Some(lineage.predecessor),
        );
        let again = layout();
        let path = write_record(&again.dir, "sa_5.json", &next);
        let env = ResumeEnv {
            root: &again.root,
            own_process_dir: OTHER_PROCESS_DIR,
            binding: &binding,
            worker_tools: None,
            planned: None,
        };
        let daemon = super::super::finalize::DAEMON_RESUME_INSTRUCTIONS;
        let full = snapshot(ResumePolicy::FULL);
        let prior = prior_run_as(&registry, "sa_2", Some(locate(&again, &path)), daemon, &env, true, full)
            .await
            .expect("the successor resumes again");
        let ResumeRetrieval::Owned { grant, evicted: gone } = &prior.retrieval else {
            panic!("a shortened history keeps owned retrieval");
        };
        assert_eq!(gone, std::slice::from_ref(&evicted));
        let mut expected = vec![
            w2.as_str().to_owned(),
            w3.as_str().to_owned(),
            s.as_str().to_owned(),
        ];
        expected.sort();
        assert_eq!(grant.allow(), expected.as_slice());
        assert_eq!(notices(&prior), 1);
        assert_eq!(last_text(&prior), format!("{daemon}\n\n{notice}"));
    }

    /// B31 (b), the sibling: the legacy store never evicts history, so a
    /// listed window it does not hold was never evicted there. The store is
    /// not the one the worker ran against, and the resume is refused before
    /// anything is reserved, by count only (mutant: drop the never-evicted
    /// check in `admit_record`).
    #[tokio::test]
    async fn b31b_absent_windows_in_a_legacy_store_refuse_the_resume_as_not_held() {
        let store = fx::legacy();
        let r = fx::run(7);
        let absent = window(0x5a);
        let w2 = fx::seal_run(&store.binding, &r, "NEEDLE second window").await;
        let w3 = fx::seal_run(&store.binding, &r, "NEEDLE third window").await;
        let rec = fx::record(
            &r,
            fx::current(&store.binding),
            vec![],
            &[absent.clone(), w2, w3],
            None,
        );
        let layout = layout();
        let path = write_record(&layout.dir, "sa_5.json", &rec);
        let binding = binding_of(&store.binding);
        let env = ResumeEnv {
            root: &layout.root,
            own_process_dir: OTHER_PROCESS_DIR,
            binding: &binding,
            worker_tools: None,
            planned: None,
        };
        let registry = Mutex::new(SubagentRegistry::new());
        let err = prior_run(&registry, "sa_1", Some(locate(&layout, &path)), "go on", &env)
            .await
            .expect_err("a store that never evicted does not hold the worker's windows")
            .to_string();
        assert!(
            err.contains("holds 1 of this worker's 3 windows nowhere and has never evicted history"),
            "{err}"
        );
        assert!(!err.contains(absent.as_str()), "{err}");
        assert!(registry.lock().unwrap().list_active().is_empty());
    }

    /// Cancel a resumed worker and wait for its thread to end.
    async fn stop(registry: &Mutex<SubagentRegistry>, handle_id: &str) {
        let thread = {
            let mut reg = registry.lock().unwrap();
            let handle = reg.get_mut(handle_id).expect("registered");
            handle.cancel();
            handle
                .thread_handle
                .take()
                .expect("the thread handle is attached before dispatch returns")
        };
        let joined = tokio::time::timeout(
            Duration::from_secs(60),
            tokio::task::spawn_blocking(move || thread.join().is_ok()),
        )
        .await
        .expect("a cancelled worker stops")
        .unwrap();
        assert!(joined, "the resumed worker ends cleanly");
    }

    /// B31 (a): the parent hears of evicted windows by count only. The tool
    /// reply's warning carries the note (after a crash checkpoint's warning,
    /// joined with "; "), the new handle counts them, and the daemon's own
    /// resume returns the count for its reload event (mutants: the note left
    /// out of the join; `evicted_windows` not set on the state;
    /// `resume_for_daemon_in` drops the count).
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    // Unix only: runs on `axel_v2` (parity-platform).
    #[cfg(unix)]
    async fn b31a_the_parent_hears_the_evicted_count_in_the_reply_and_the_daemon_resume() {
        // Real worker threads start: an empty base directory and no provider
        // credentials, so they fail at auth, offline.
        let _base = crate::test_env::BaseDirGuard::new();
        let _offline = crate::test_env::OfflineProviderEnv::new();
        let r = fx::run(7);
        let (evicted, w2) = (window(0x5a), window(0x72));
        let fake = axel_v2(&[(&w2, &r)], 3);
        let mut rec = fx::record(
            &r,
            fx::current(&fake.binding),
            vec![],
            &[evicted.clone(), w2],
            None,
        );
        // A model the session's catalog knows.
        rec.meta.model = "anthropic/claude-sonnet-4-6".into();
        let mut head = rec.clone();
        head.kind = RecordKind::Head;
        // Its own handle: a record file names the handle it belongs to.
        head.handle_id = "sa_6".into();
        let layout = layout();
        let final_path = write_record(&layout.dir, "sa_5.json", &rec);
        write_record(&layout.dir, "sa_6.head.json", &head);
        let tools = crate::ToolRegistry::without_subagent();
        let binding = binding_of(&fake.binding);
        let env = ResumeEnv {
            root: &layout.root,
            own_process_dir: OTHER_PROCESS_DIR,
            binding: &binding,
            worker_tools: Some(&tools),
            planned: None,
        };
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        let note = super::super::lineage::evicted_parent_note(1).unwrap();
        let crash = super::super::admission::CRASH_CHECKPOINT_WARNING;
        for (path, expected) in [
            (final_path.clone(), note.clone()),
            // Named by its final's path: the head is found as its sibling.
            (layout.dir.join("sa_6.json"), format!("{crash}; {note}")),
        ] {
            let mut ctx = create_tool_context();
            ctx.capabilities.subagent_registry = Some(Arc::clone(&registry));
            let reply = SubagentResumeTool
                .execute_in(
                    json!({"handle_id": "sa_1", "instructions": "go on",
                           "archive_path": path.to_str().unwrap()}),
                    ctx,
                    &env,
                )
                .await
                .expect("resumed");
            assert!(!reply.contains(evicted.as_str()), "{reply}");
            let body: Value = serde_json::from_str(&reply).unwrap();
            assert_eq!(body["warning"], expected.as_str(), "{reply}");
            let handle_id = body["handle_id"].as_str().unwrap().to_owned();
            let counted = registry.lock().unwrap().get(&handle_id).unwrap().evicted_windows();
            assert_eq!(counted, 1);
            stop(&registry, &handle_id).await;
        }

        // The daemon resumes a reload-stopped worker of an earlier process.
        registry
            .lock()
            .unwrap()
            .adopt_carried(&crate::runtime::subagent::CarriedWorker {
                id: "sa_5".into(),
                status: "cancelled".into(),
                archive_path: Some(final_path),
                stopped_by_reload: true,
                wake: true,
                ..Default::default()
            });
        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(Arc::clone(&registry));
        let (new, _, evicted_windows) = resume_for_daemon_in(ctx, "sa_5", None, &env)
            .await
            .expect("the daemon resumes the worker");
        assert_eq!(evicted_windows, 1);
        let counted = registry.lock().unwrap().get(&new).unwrap().evicted_windows();
        assert_eq!(counted, 1);
        stop(&registry, &new).await;
    }

    #[tokio::test]
    async fn w8_owned_history_install_is_strictly_additive() {
        let store = fx::legacy();
        let r = fx::run(6);
        let w1 = fx::seal_run(&store.binding, &r, "first").await;
        let rec = fx::record(&r, fx::current(&store.binding), vec![], &[w1], None);
        let grant = fx::grant(&rec, &store.binding).await;
        let mut tools = crate::ToolRegistry::without_subagent();
        assert!(tools.get("memory_search").is_none());
        super::super::retrieval::install_owned_history(&mut tools, Arc::clone(&grant)).unwrap();
        let installed = Arc::clone(tools.get("memory_fetch").unwrap());
        assert_eq!(installed.origin(), crate::tools::ToolOrigin::Builtin);
        let err = super::super::retrieval::install_owned_history(&mut tools, grant).unwrap_err();
        assert!(err.contains("could not be installed"), "{err}");
        assert!(Arc::ptr_eq(&installed, tools.get("memory_fetch").unwrap()));
    }

    #[tokio::test]
    async fn w10_an_owned_registry_that_cannot_be_built_refuses_before_anything_is_reserved() {
        let store = fx::legacy();
        let r = fx::run(10);
        let w1 = fx::seal_run(&store.binding, &r, "first").await;
        let rec = fx::record(&r, fx::current(&store.binding), vec![], &[w1], None);
        let layout = layout();
        let path = write_record(&layout.dir, "sa_5.json", &rec);
        // A worker registry that already holds builtin memory tools.
        let mut template = crate::ToolRegistry::without_subagent();
        let grant = fx::grant(&rec, &store.binding).await;
        super::super::retrieval::install_owned_history(&mut template, grant).unwrap();
        let binding = binding_of(&store.binding);
        let env = ResumeEnv {
            root: &layout.root,
            own_process_dir: OTHER_PROCESS_DIR,
            binding: &binding,
            worker_tools: Some(&template),
            planned: None,
        };
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(Arc::clone(&registry));
        ctx.capabilities.memory_backend = Some(store.binding.clone());
        let orchestration = Arc::clone(ctx.capabilities.orchestration.as_ref().unwrap());
        let gate = orchestration.completion_gate();
        let err = SubagentResumeTool
            .execute_in(
                json!({"handle_id": "sa_5", "instructions": "x",
                       "archive_path": path.to_str().unwrap()}),
                ctx,
                &env,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("owned history tools could not be installed"),
            "{err}"
        );
        assert!(err.contains("Nothing was started"), "{err}");
        assert!(registry.lock().unwrap().list_active().is_empty());
        assert_eq!(orchestration.delegation_descendants(), 0);
        assert_eq!(orchestration.completion_gate(), gate);
    }

    /// A finished worker of this process whose archive write failed is
    /// retried before its context is looked up, on a blocking worker with no
    /// registry guard held; the resume continues the archive it wrote.
    #[tokio::test]
    async fn w11_a_failed_archive_is_retried_off_the_registry_before_resume() {
        let layout = layout();
        let registry = Mutex::new(SubagentRegistry::new());
        let h = finished_handle("sa_48", SubagentStatus::Completed, None);
        let state = Arc::clone(&h.state);
        {
            let mut s = state.write().unwrap();
            s.finalization_ready = true;
            s.archive_meta = Some(crate::runtime::subagent::SubagentArchiveMeta {
                agent_name: "archived-label".into(),
                model: "anthropic/claude-opus-5-5".into(),
                system_prompt: "archived system".into(),
                timeout_secs: 3500,
                cwd: None,
                thinking: None,
            watch_usage: None,
            });
            s.archive_dir = Some(layout.dir.clone());
            s.history = Some(vec![
                Arc::new(json!({"role": "user", "content": "build the plate"})),
                Arc::new(json!({"role": "assistant", "content": "working"})),
            ]);
            s.archive_error =
                Some("archive write failed: disk full; full history retained in memory".into());
        }
        registry.lock().unwrap().register(h);
        let (reached, open) = super::super::archive::retry_gate::gate("sa_48");
        let control = async {
            tokio::task::spawn_blocking(move || reached.recv().unwrap())
                .await
                .unwrap();
            assert!(registry.try_lock().is_ok(), "no registry guard during the write");
            open.send(()).unwrap();
        };
        let env = env(&layout);
        let (prior, ()) = tokio::join!(prior_run(&registry, "sa_48", None, "x", &env), control);
        let prior = prior.unwrap();
        assert_eq!(prior.messages[0]["content"], "build the plate");
        let s = state.read().unwrap();
        assert_eq!(s.archive_path, Some(layout.dir.join("sa_48.json")));
        assert_eq!(s.archive_error, None);
        assert!(s.history.is_none());
    }

    /// Concurrent observation (transient, documented on `ArchiveRetry`): a
    /// resume that finds a collect's retry of the same worker in flight does
    /// not wait for it. It continues the finalized history kept in memory
    /// (the history that retry is writing), before the archive exists; the
    /// retry still lands.
    #[tokio::test]
    async fn w12_a_resume_during_a_collects_retry_continues_the_history_in_memory() {
        let layout = layout();
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        let h = finished_handle("sa_49", SubagentStatus::Completed, None);
        let state = Arc::clone(&h.state);
        {
            let mut s = state.write().unwrap();
            s.finalization_ready = true;
            s.archive_meta = Some(crate::runtime::subagent::SubagentArchiveMeta {
                agent_name: "archived-label".into(),
                model: "anthropic/claude-opus-5-5".into(),
                system_prompt: "archived system".into(),
                timeout_secs: 3500,
                cwd: None,
                thinking: None,
            watch_usage: None,
            });
            s.archive_dir = Some(layout.dir.clone());
            s.history = Some(vec![
                Arc::new(json!({"role": "user", "content": "build the plate"})),
                Arc::new(json!({"role": "assistant", "content": "working"})),
            ]);
            s.archive_error =
                Some("archive write failed: disk full; full history retained in memory".into());
        }
        registry.lock().unwrap().register(h);
        let (reached, open) = super::super::archive::retry_gate::gate("sa_49");
        let mut ctx = crate::tools::test_helpers::create_tool_context();
        ctx.capabilities.subagent_registry = Some(Arc::clone(&registry));
        // Registered directly, never dispatched through orchestration.
        ctx.capabilities.orchestration = None;
        let collect = tokio::spawn(async move {
            super::super::collect::SubagentCollectTool
                .execute(json!({"handle_id": "sa_49"}), ctx)
                .await
        });
        tokio::task::spawn_blocking(move || reached.recv().unwrap())
            .await
            .unwrap();
        let env = env(&layout);
        let prior = prior_run(&registry, "sa_49", None, "x", &env).await.unwrap();
        assert!(state.read().unwrap().archive_retrying, "still writing");
        assert!(!layout.dir.join("sa_49.json").exists(), "not from the archive");
        // The two messages kept in memory, then the resume instructions.
        assert_eq!(prior.messages.len(), 3);
        assert_eq!(prior.messages[0]["content"], "build the plate");
        assert_eq!(prior.messages[1]["content"], "working");
        assert!(prior.messages[2].to_string().contains("[Resumed by the orchestrator."));
        assert!(prior.lineage.is_none());

        open.send(()).unwrap();
        collect.await.unwrap().unwrap();
        let s = state.read().unwrap();
        assert_eq!(s.archive_path, Some(layout.dir.join("sa_49.json")));
        assert_eq!(s.archive_error, None);
    }

    /// Strict r1 B-B2, the resume half: a v2 final's watch usage (what a
    /// watch worker's final carries since the fix, persist_tests' b2_*)
    /// is charged at an explicit-path resume: a spent lease is refused,
    /// with nothing reserved.
    #[tokio::test]
    async fn w13_an_explicit_path_resume_of_a_v2_final_with_a_spent_watch_lease_is_refused() {
        let store = fx::legacy();
        let r = fx::run(13);
        let own = fx::seal_run(&store.binding, &r, "own").await;
        let mut rec = fx::record(
            &r,
            fx::current(&store.binding),
            vec![],
            std::slice::from_ref(&own),
            None,
        );
        rec.meta.watch_usage = Some(super::super::record::WorkerWatchUsage {
            secs: 7200,
            rounds: 3,
        });
        // A model the session's catalog knows (the dispatch is authorized
        // before the lease is charged).
        rec.meta.model = "anthropic/claude-sonnet-4-6".into();
        let layout = layout();
        let path = write_record(&layout.dir, "sa_5.json", &rec);
        let binding = binding_of(&store.binding);
        let env = ResumeEnv {
            root: &layout.root,
            own_process_dir: OTHER_PROCESS_DIR,
            binding: &binding,
            worker_tools: None,
            planned: None,
        };
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(Arc::clone(&registry));
        ctx.capabilities.memory_backend = Some(store.binding.clone());
        let orchestration = Arc::clone(ctx.capabilities.orchestration.as_ref().unwrap());
        let gate = orchestration.completion_gate();
        let text = SubagentResumeTool
            .execute_in(
                json!({"handle_id": "sa_1", "instructions": "check the run again",
                   "archive_path": path.to_str().unwrap()}),
                ctx,
                &env,
            )
            .await
            .expect("a refusal is a tool result");
        assert!(
            text.starts_with("subagent_resume refused (watch lease expired"),
            "{text}"
        );
        assert!(text.contains("7200s of 7200s"), "{text}");
        assert!(registry.lock().unwrap().list_active().is_empty());
        assert_eq!(orchestration.delegation_descendants(), 0);
        assert_eq!(orchestration.completion_gate(), gate);
    }
}

/// Warn, never silently extend an explicitly requested wall-clock limit.
fn short_timeout_warning(seconds: u64) -> Option<&'static str> {
    (seconds > 0 && seconds < 300).then_some(
        "Resume timeout is below 300s and may expire during upload or API retries; prefer at least 600s (0 disables the limit)."
    )
}

#[cfg(test)]
mod short_timeout_tests {
    #[test]
    fn only_short_positive_effective_limits_warn() {
        for (seconds, warns) in [
            (0, false),
            (60, true),
            (299, true),
            (300, false),
            (600, false),
        ] {
            assert_eq!(super::short_timeout_warning(seconds).is_some(), warns);
        }
    }
}
