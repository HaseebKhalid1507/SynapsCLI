//! Small-watcher admission: before a subagent starts, resumes or runs
//! oneshot, decide whether its task is a polling job ("wait until X, then
//! report") on a model too large for it. Such a job is refused with a ready
//! `wait` call in its place; the same job on a small model is allowed as a
//! watch worker, under a finite lease.
//!
//! Deterministic, no I/O. The classifier reads the worker's name (at most
//! [`NAME_BYTES`]) and the first [`TASK_BYTES`] of its task, each cut on a
//! char boundary, so its cost is bounded by that input (the regex crate runs
//! in linear time; the patterns are compiled once).
//!
//! Rules (ids are logged, never task text):
//! - P1 (strong): the name says poll/poller/wait/waiter/watch/watcher.
//! - P2 (strong): `sleep N` together with an observation command (tail, cat,
//!   grep, ls, stat, ps, pgrep, fleet-wait, status).
//! - P3a..P3g (weak): "poll", "check every", "every N s/min", "keep checking",
//!   "wait for/until", "until … exists/appears/contains/finishes/DONE",
//!   "report when".
//! - W1 (counter): at least [`WORK_HITS`] work verbs (implement, fix, write,
//!   edit, commit, review, refactor, design, build, test), with the shell's
//!   `test -X` not counted.
//! - W2 (counter): a write_policy other than read_only, only when the caller
//!   set one (the default policy is not evidence).
//!
//! Class: Poll = a strong rule and no counter. Ambiguous = a strong rule with
//! a counter, or weak rules only. None otherwise.

use regex::Regex;
use std::sync::OnceLock;

/// Bytes of the worker's name the classifier reads.
pub const NAME_BYTES: usize = 128;
/// Bytes of the task the classifier reads.
pub const TASK_BYTES: usize = 8192;
/// Work-verb hits that make a task more than a poll (calibrated on the
/// labelled dispatch corpus).
pub const WORK_HITS: usize = 4;

/// The tracing target of every admission and watch-lease line (here and in
/// `drive.rs`). It is under `agent_engine`, so the daemon log's filter
/// (`agent_core::logging::with_crate_directives`, `agent_engine=debug`)
/// keeps the lines with `RUST_LOG` unset; a target outside Synaps's crates
/// is dropped below ERROR. A `RUST_LOG` that named the earlier bare
/// `subagent_admission` target must name this one.
pub const LOG_TARGET: &str = "agent_engine::subagent_admission";

/// The poll shape of a dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollClass {
    Poll,
    Ambiguous,
    None,
}

impl PollClass {
    pub fn as_str(self) -> &'static str {
        match self {
            PollClass::Poll => "poll",
            PollClass::Ambiguous => "ambiguous",
            PollClass::None => "none",
        }
    }
}

/// What the classifier saw: the class and the rule and counter ids that hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub class: PollClass,
    pub rules: Vec<&'static str>,
    pub counters: Vec<&'static str>,
}

/// One dispatch as admission sees it.
#[derive(Debug, Clone, Copy)]
pub struct WatchRequest<'a> {
    pub name: Option<&'a str>,
    pub task: &'a str,
    /// The write_policy mode, only when the caller passed one.
    pub explicit_write_policy: Option<&'a str>,
}

/// Cut `s` to at most `max` bytes on a char boundary.
pub fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

struct Patterns {
    p1: Regex,
    p2_sleep: Regex,
    p2_obs: Regex,
    p3: Vec<(&'static str, Regex)>,
    shell_test: Regex,
    work: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let rx = |s: &str| Regex::new(s).expect("admission pattern");
        Patterns {
            p1: rx(r"(?i)(^|[-_ ])(poll|poller|wait|waiter|watch|watcher)([-_ 0-9]|$)"),
            p2_sleep: rx(r"(?i)\bsleep\s+\d+"),
            p2_obs: rx(r"(?i)\b(tail|cat|grep|ls|stat|ps|pgrep|fleet-wait|status)\b"),
            p3: vec![
                ("P3a", rx(r"(?i)\bpoll(s|ing|ed)?\b")),
                ("P3b", rx(r"(?i)\bcheck every\b")),
                ("P3c", rx(r"(?i)\bevery\s+\d+\s*(s|sec|secs|seconds|m|min|mins|minutes)\b")),
                ("P3d", rx(r"(?i)\bkeep checking\b")),
                ("P3e", rx(r"(?i)\bwait\s+(for|until)\b")),
                ("P3f", rx(r"\buntil\b[^.\n]{0,80}\b(exists|appears|contains|finishes|DONE)\b")),
                ("P3g", rx(r"(?i)\breport when\b")),
            ],
            shell_test: rx(r"\btest\s+-[a-zA-Z]\b"),
            work: rx(
                r"(?i)\b(implement|fix|write|edit|commit|review|refactor|design|build|test)(s|es|ed|ing)?\b",
            ),
        }
    })
}

/// Classify one dispatch.
pub fn classify(req: &WatchRequest<'_>) -> Classification {
    let p = patterns();
    let name = cut(req.name.unwrap_or(""), NAME_BYTES);
    let task = cut(req.task, TASK_BYTES);
    let mut rules = Vec::new();
    if p.p1.is_match(name) {
        rules.push("P1");
    }
    if p.p2_sleep.is_match(task) && p.p2_obs.is_match(task) {
        rules.push("P2");
    }
    for (id, rx) in &p.p3 {
        if rx.is_match(task) {
            rules.push(*id);
        }
    }
    let mut counters = Vec::new();
    let without_shell_tests = p.shell_test.replace_all(task, " ");
    if p.work
        .find_iter(&without_shell_tests)
        .take(WORK_HITS)
        .count()
        >= WORK_HITS
    {
        counters.push("W1");
    }
    if req
        .explicit_write_policy
        .is_some_and(|mode| mode != "read_only")
    {
        counters.push("W2");
    }
    let strong = rules.iter().any(|r| *r == "P1" || *r == "P2");
    let weak = rules.iter().any(|r| r.starts_with("P3"));
    let class = if strong && counters.is_empty() {
        PollClass::Poll
    } else if strong || weak {
        PollClass::Ambiguous
    } else {
        PollClass::None
    };
    Classification {
        class,
        rules,
        counters,
    }
}

/// The ready `wait` call a refusal offers. Only values the task names
/// unambiguously go in (exactly one of a kind: one absolute path, one
/// `sa_N` handle, one pid); otherwise the generic shape.
pub fn wait_hint(task: &str) -> String {
    let task = cut(task, TASK_BYTES);
    static RX: OnceLock<(Regex, Regex, Regex, Regex)> = OnceLock::new();
    let (path_rx, handle_rx, pid_rx, contains_rx) = RX.get_or_init(|| {
        (
            Regex::new(r#"(?:^|[\s"'`(=])(/[A-Za-z0-9._-]+(?:/[A-Za-z0-9._-]+)+)"#).unwrap(),
            Regex::new(r"\b(sa_[0-9]{1,9})\b").unwrap(),
            Regex::new(r"(?i)\bpid\s*[:=]?\s*([0-9]{1,9})\b").unwrap(),
            Regex::new(r#"--contains\s+(?:"([^"\n]{1,120})"|'([^'\n]{1,120})'|([^\s"']{1,120}))"#)
                .unwrap(),
        )
    });
    let unique = |rx: &Regex| -> Option<String> {
        // A sentence's full stop is not part of a path.
        let mut found: Vec<String> = rx
            .captures_iter(task)
            .filter_map(|c| {
                c.get(1)
                    .map(|m| m.as_str().trim_end_matches('.').to_owned())
            })
            .collect();
        found.sort();
        found.dedup();
        (found.len() == 1).then(|| found.remove(0))
    };
    let contains = {
        let mut found: Vec<String> = contains_rx
            .captures_iter(task)
            .filter_map(|c| {
                c.get(1)
                    .or(c.get(2))
                    .or(c.get(3))
                    .map(|m| m.as_str().to_owned())
            })
            .collect();
        found.sort();
        found.dedup();
        (found.len() == 1).then(|| found.remove(0))
    };
    let mut conditions = Vec::new();
    if let Some(path) = unique(path_rx) {
        let mut c = serde_json::json!({ "file": path });
        if let Some(text) = contains {
            c["contains"] = serde_json::Value::String(text);
        }
        conditions.push(c);
    }
    if let Some(handle) = unique(handle_rx) {
        conditions.push(serde_json::json!({ "subagent": handle }));
    }
    if let Some(pid) = unique(pid_rx).and_then(|p| p.parse::<u64>().ok()) {
        conditions.push(serde_json::json!({ "pid": pid }));
    }
    if conditions.is_empty() {
        return r#"wait {"any": [{"file": "<path>", "contains": "<text>"}], "timeout_secs": 3600}"#
            .to_owned();
    }
    format!(
        "wait {}",
        serde_json::json!({ "any": conditions, "timeout_secs": 3600 })
    )
}

/// The refusal's tool result.
pub fn refusal_text(entry: &str, task: &str, small_model: &str) -> String {
    format!(
        "{entry} refused (polling task): a worker that only waits and reports spends a \
         model round per check. Wait in this turn instead, in one blocking call:\n  {}\n\
         or start it on {small_model} if it needs judgement.",
        wait_hint(task)
    )
}

/// The advisor's answer for an ambiguous dispatch.
#[derive(Debug, Clone, PartialEq)]
pub struct Advice {
    /// event_wait | bounded_watcher | inspect | unknown
    pub choice: String,
    pub confidence: f64,
}

/// Asked at most once per ambiguous dispatch, within the configured budget.
/// `None`: no advisor, an error or a timeout (the dispatch is allowed).
#[async_trait::async_trait]
pub trait Advisor: Send + Sync {
    async fn advise(&self, request: serde_json::Value) -> Option<Advice>;
}

/// What admission decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Admission {
    /// Start it. `watch`: a polling worker on a small model, under the lease.
    Allow { watch: bool },
    /// Do not start it; the text is the tool result.
    Refuse(String),
}

/// One dispatch, with what the entry point already resolved.
#[derive(Debug, Clone, Copy)]
pub struct Dispatch<'a> {
    pub request: WatchRequest<'a>,
    pub requested_model: Option<&'a str>,
    pub resolved_model: &'a str,
}

/// The advisor request (`v` 1): metadata, the rule ids and a bounded,
/// control-free excerpt of the task.
pub fn advice_request(d: &Dispatch<'_>, c: &Classification) -> serde_json::Value {
    let excerpt: String = d
        .request
        .task
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n')
        .take(2000)
        .collect();
    serde_json::json!({
        "v": 1,
        "kind": "poll_shape",
        "name": cut(d.request.name.unwrap_or(""), NAME_BYTES),
        "task_excerpt": excerpt,
        "requested_model": d.requested_model,
        "resolved_model": d.resolved_model,
        "rule_hits": c.rules,
        "counter_hits": c.counters,
        "choices": ["event_wait", "bounded_watcher", "inspect", "unknown"],
    })
}

/// Decide, given the configuration and an optional advisor. Logs one
/// metadata-only line (target [`LOG_TARGET`]); never task text.
pub async fn admit(
    entry: &'static str,
    d: &Dispatch<'_>,
    cfg: &crate::config::WatchConfig,
    advisor: Option<&dyn Advisor>,
) -> Admission {
    let c = classify(&d.request);
    let small = cfg.small_models.iter().any(|m| m == d.resolved_model);
    let mut advice = None;
    let action = if cfg.admission == crate::config::WatchAdmission::Off {
        Admission::Allow {
            watch: small && c.class != PollClass::None,
        }
    } else if small {
        Admission::Allow {
            watch: c.class != PollClass::None,
        }
    } else {
        match c.class {
            PollClass::None => Admission::Allow { watch: false },
            PollClass::Poll => {
                Admission::Refuse(refusal_text(entry, d.request.task, suggested(cfg)))
            }
            PollClass::Ambiguous => {
                advice = match advisor {
                    Some(a) if cfg.advice_timeout_ms > 0 => tokio::time::timeout(
                        std::time::Duration::from_millis(cfg.advice_timeout_ms),
                        a.advise(advice_request(d, &c)),
                    )
                    .await
                    .ok()
                    .flatten(),
                    _ => None,
                };
                match &advice {
                    Some(a)
                        if a.confidence >= cfg.advice_min_confidence
                            && matches!(a.choice.as_str(), "event_wait" | "bounded_watcher") =>
                    {
                        Admission::Refuse(refusal_text(entry, d.request.task, suggested(cfg)))
                    }
                    _ => Admission::Allow { watch: false },
                }
            }
        }
    };
    tracing::info!(
        target: LOG_TARGET,
        entry,
        poll_class = c.class.as_str(),
        rule_ids = ?c.rules,
        counter_ids = ?c.counters,
        advice = ?advice.as_ref().map(|a| (a.choice.as_str(), a.confidence)),
        action = match &action {
            Admission::Refuse(_) => "refuse",
            Admission::Allow { watch: true } => "allow_watch",
            Admission::Allow { watch: false } => "allow",
        },
        resolved_model = d.resolved_model,
        "subagent admission"
    );
    action
}

/// The model a refusal suggests: the first configured small model.
fn suggested(cfg: &crate::config::WatchConfig) -> &str {
    cfg.small_models
        .first()
        .map(String::as_str)
        .unwrap_or(crate::config::DEFAULT_WATCH_SMALL_MODEL)
}

pub use crate::runtime::subagent::WatchUsage;

/// What is left of a watch worker's lease for its next run. Operational
/// bounds, not spend caps: the cost part is unset unless the operator set
/// `subagent.watch.lease_cost_usd`, and then applies per run (time and
/// rounds carry across runs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WatchLease {
    pub secs: u64,
    pub rounds: u32,
    pub cost_usd: Option<f64>,
}

impl WatchLease {
    /// The lease left after `used`, or the refusal text when it is spent.
    pub fn remaining(cfg: &crate::config::WatchConfig, used: WatchUsage) -> Result<Self, String> {
        let secs = cfg.lease_secs.saturating_sub(used.secs);
        let rounds = cfg.lease_rounds.saturating_sub(used.rounds);
        if secs == 0 || rounds == 0 {
            return Err(format!(
                "watch lease expired: this watch worker has used {}s of {}s and {} of {} provider rounds",
                used.secs.min(cfg.lease_secs),
                cfg.lease_secs,
                used.rounds.min(cfg.lease_rounds),
                cfg.lease_rounds
            ));
        }
        Ok(Self {
            secs,
            rounds,
            cost_usd: cfg.lease_cost_usd,
        })
    }

    /// The worker's wall-clock limit: the caller's (0 = none) clamped to the
    /// lease. The drive enforces it during blocking tools too (its deadline
    /// races the worker's stream and cancels the worker).
    pub fn clamp_timeout(&self, requested_secs: u64) -> u64 {
        if requested_secs == 0 {
            self.secs
        } else {
            requested_secs.min(self.secs)
        }
    }

    /// Bound the worker's turn by the rounds left (no renewals: the lease is
    /// the bound) and, when set, the cost lease. Returns whether the lease's
    /// rounds are the turn's round bound (false when an operator's lower
    /// worker cap is): only then is a round stop the lease's.
    pub fn apply_to(&self, budget: &mut crate::runtime::budget::TurnBudget) -> bool {
        let binds = budget
            .max_provider_rounds
            .is_none_or(|max| self.rounds <= max);
        budget.max_provider_rounds = Some(
            budget
                .max_provider_rounds
                .map_or(self.rounds, |max| max.min(self.rounds)),
        );
        budget.max_round_renewals = 0;
        if let Some(usd) = self.cost_usd {
            budget.max_cost_usd = Some(budget.max_cost_usd.map_or(usd, |max| max.min(usd)));
        }
        binds
    }
}

/// An admitted dispatch: a watch worker's lease and its usage so far.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Admitted {
    pub watch: Option<(WatchLease, WatchUsage)>,
}

/// The one admission call an entry point makes, after it resolved and
/// authorized the model. `prior`: the usage of the watch worker a resume
/// continues (a watch worker stays one, whatever its new instructions say).
/// `Err` is the tool result of a refusal; the caller releases what it
/// reserved.
pub async fn admit_dispatch(
    entry: &'static str,
    d: &Dispatch<'_>,
    prior: Option<WatchUsage>,
    advisor: Option<&dyn Advisor>,
) -> Result<Admitted, String> {
    let cfg = tokio::task::spawn_blocking(|| crate::config::load_config().watch)
        .await
        .unwrap_or_default();
    decide_dispatch(entry, d, &cfg, prior, advisor).await
}

/// [`admit_dispatch`] with the configuration given.
pub async fn decide_dispatch(
    entry: &'static str,
    d: &Dispatch<'_>,
    cfg: &crate::config::WatchConfig,
    prior: Option<WatchUsage>,
    advisor: Option<&dyn Advisor>,
) -> Result<Admitted, String> {
    if let Some(used) = prior {
        return match WatchLease::remaining(cfg, used) {
            Ok(lease) => Ok(Admitted {
                watch: Some((lease, used)),
            }),
            Err(why) => {
                tracing::info!(
                    target: LOG_TARGET,
                    entry,
                    action = "refuse_lease_expired",
                    used_secs = used.secs,
                    used_rounds = used.rounds,
                    resolved_model = d.resolved_model,
                    "subagent admission"
                );
                Err(format!(
                    "{entry} refused ({why}). Wait in this turn instead, in one blocking call:\n  {}\n\
                     or start a fresh watcher on {} with a short brief.",
                    wait_hint(d.request.task),
                    suggested(cfg)
                ))
            }
        };
    }
    match admit(entry, d, cfg, advisor).await {
        Admission::Refuse(text) => Err(text),
        Admission::Allow { watch: false } => Ok(Admitted { watch: None }),
        Admission::Allow { watch: true } => {
            // The parser keeps lease_secs >= 60 and lease_rounds >= 1; a
            // config built by hand still gets a lease of at least one of each.
            let lease = WatchLease {
                secs: cfg.lease_secs.max(1),
                rounds: cfg.lease_rounds.max(1),
                cost_usd: cfg.lease_cost_usd,
            };
            Ok(Admitted {
                watch: Some((lease, WatchUsage::default())),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(name: Option<&'a str>, task: &'a str) -> WatchRequest<'a> {
        WatchRequest {
            name,
            task,
            explicit_write_policy: None,
        }
    }

    #[test]
    fn a_named_waiter_with_no_work_is_a_poll() {
        let c = classify(&req(
            Some("t-lease-wait"),
            "Block until the rc file exists, then report it.",
        ));
        assert_eq!(c.class, PollClass::Poll, "{c:?}");
        assert!(c.rules.contains(&"P1"));
        assert!(c.counters.is_empty());
    }

    #[test]
    fn sleep_with_an_observation_command_is_strong() {
        let c = classify(&req(
            None,
            "loop: sleep 60; tail -5 /var/tmp/x.log; stop at DONE",
        ));
        assert_eq!(c.class, PollClass::Poll, "{c:?}");
        assert!(c.rules.contains(&"P2"));
        // sleep alone is not P2
        assert!(!classify(&req(None, "sleep 60 then summarise"))
            .rules
            .contains(&"P2"));
    }

    #[test]
    fn four_work_verbs_make_a_strong_match_ambiguous_and_shell_tests_do_not_count() {
        let work = "Implement the fix, write the test, commit it, then review the diff.";
        let c = classify(&req(Some("watcher"), work));
        assert_eq!(c.class, PollClass::Ambiguous, "{c:?}");
        assert_eq!(c.counters, vec!["W1"]);
        // Three hits are not enough.
        let c = classify(&req(Some("watcher"), "fix it, test it, commit it"));
        assert_eq!(c.class, PollClass::Poll, "{c:?}");
        // `test -f` / `test -e` are shell checks, not work.
        let c = classify(&req(
            Some("watcher"),
            "until test -f /a; do sleep 5; done; test -e /b; test -s /c; test -d /d",
        ));
        assert_eq!(c.class, PollClass::Poll, "{c:?}");
    }

    #[test]
    fn only_an_explicit_write_policy_counts() {
        let mut r = req(Some("poller"), "wait until the file exists");
        assert_eq!(classify(&r).class, PollClass::Poll);
        r.explicit_write_policy = Some("read_only");
        assert_eq!(classify(&r).class, PollClass::Poll);
        r.explicit_write_policy = Some("isolated_worktree");
        let c = classify(&r);
        assert_eq!(c.class, PollClass::Ambiguous);
        assert_eq!(c.counters, vec!["W2"]);
    }

    #[test]
    fn weak_rules_alone_are_ambiguous_and_plain_work_is_none() {
        let c = classify(&req(
            Some("reviewer"),
            "Review the diff. Wait for CI and report when it is green.",
        ));
        assert_eq!(c.class, PollClass::Ambiguous, "{c:?}");
        assert!(
            c.rules.contains(&"P3e") && c.rules.contains(&"P3g"),
            "{c:?}"
        );
        let c = classify(&req(
            Some("implementer"),
            "Implement the parser and its tests.",
        ));
        assert_eq!(c.class, PollClass::None, "{c:?}");
        assert!(c.rules.is_empty());
    }

    #[test]
    fn the_name_rule_needs_a_word_boundary() {
        for name in ["waiter", "t-chain-wait", "watch_2", "poll", "lease waiter"] {
            assert!(
                classify(&req(Some(name), "x")).rules.contains(&"P1"),
                "{name}"
            );
        }
        for name in ["waitress", "stopwatch", "pollster", "awaited"] {
            assert!(
                !classify(&req(Some(name), "x")).rules.contains(&"P1"),
                "{name}"
            );
        }
    }

    #[test]
    fn only_the_inspected_prefix_is_read() {
        let mut task = "a".repeat(TASK_BYTES);
        task.push_str(" sleep 30; tail log");
        assert!(classify(&req(None, &task)).rules.is_empty());
        // A multi-byte char at the cut is not split.
        let task = format!("{}é sleep 5", "a".repeat(TASK_BYTES - 1));
        assert_eq!(cut(&task, TASK_BYTES).len(), TASK_BYTES - 1);
        let long_name = format!("{}-watcher", "n".repeat(NAME_BYTES));
        assert!(!classify(&req(Some(&long_name), "x")).rules.contains(&"P1"));
    }

    #[test]
    fn the_hint_offers_only_unambiguous_values() {
        let h = wait_hint("Run fleet-wait --file /var/tmp/x/h2a.rc --contains 'rc=' and report.");
        assert!(h.contains(r#""file":"/var/tmp/x/h2a.rc""#), "{h}");
        assert!(h.contains(r#""contains":"rc=""#), "{h}");
        let h = wait_hint("Poll sa_31 until it finishes");
        assert!(h.contains(r#""subagent":"sa_31""#), "{h}");
        let h = wait_hint("watch pid 4242 until it exits");
        assert!(h.contains(r#""pid":4242"#), "{h}");
        // Two paths: neither is offered.
        let h = wait_hint("wait for /var/tmp/a/x.rc or /var/tmp/b/y.rc");
        assert!(h.contains("<path>"), "{h}");
        assert!(!h.contains("/var/tmp"), "{h}");
        // A relative path is not offered.
        assert!(wait_hint("wait for out/x.rc").contains("<path>"));
        // The sentence's full stop is not part of the path, and the same
        // path named twice is still one path.
        let h = wait_hint("Wait for /var/tmp/a/x.rc. Then read /var/tmp/a/x.rc again.");
        assert!(h.contains(r#""file":"/var/tmp/a/x.rc"}"#), "{h}");
    }

    #[test]
    fn the_refusal_text_starts_with_its_marker_and_names_the_small_model() {
        let t = refusal_text(
            "subagent_start",
            "wait for /var/tmp/a/x.rc",
            "muse/muse-wonder-1.0",
        );
        assert!(
            t.starts_with("subagent_start refused (polling task):"),
            "{t}"
        );
        assert!(
            t.contains(r#"wait {"any":[{"file":"/var/tmp/a/x.rc"}],"timeout_secs":3600}"#),
            "{t}"
        );
        assert!(t.contains("muse/muse-wonder-1.0"), "{t}");
    }

    #[test]
    fn the_worst_case_input_classifies_within_its_bound() {
        // Every rule near-missing, the full inspected length.
        let unit = "pol wai for sleep x tai every 5 hours untilexists report whe test -f ";
        let task: String = unit.repeat(TASK_BYTES / unit.len() + 1);
        let name = "w".repeat(NAME_BYTES);
        let r = req(Some(&name), &task);
        let _ = classify(&r); // compile the patterns outside the timing
        let started = std::time::Instant::now();
        let n = 200;
        for _ in 0..n {
            std::hint::black_box(classify(&r));
        }
        let per = started.elapsed() / n;
        // Loose in CI (debug build, shared machine); the report gives p50/p99.
        assert!(
            per < std::time::Duration::from_millis(20),
            "{per:?} per classification"
        );
    }

    #[test]
    fn the_lease_carries_usage_and_bounds_time_rounds_and_set_cost() {
        let cfg = crate::config::WatchConfig::default();
        let l = WatchLease::remaining(&cfg, WatchUsage::default()).unwrap();
        assert_eq!((l.secs, l.rounds, l.cost_usd), (7200, 40, None));
        let used = WatchUsage::default().plus(7000, 30).plus(100, 5);
        let l = WatchLease::remaining(&cfg, used).unwrap();
        assert_eq!((l.secs, l.rounds), (100, 5), "both runs count");
        assert_eq!(l.clamp_timeout(0), 100, "no limit becomes the lease");
        assert_eq!(l.clamp_timeout(3600), 100);
        assert_eq!(l.clamp_timeout(30), 30);
        let mut b =
            crate::runtime::budget::TurnBudget::for_role(crate::runtime::budget::TurnRole::Worker);
        assert!(
            l.apply_to(&mut b),
            "5 left, under the worker's 64: the lease binds"
        );
        assert_eq!(b.max_provider_rounds, Some(5));
        assert_eq!(
            b.max_round_renewals, 0,
            "the lease is the bound, no renewals"
        );
        assert_eq!(
            b.max_cost_usd, None,
            "no cost lease unless set (no default spend cap)"
        );
        // An operator's lower worker cap stays, and a round stop is then not the lease's.
        let mut b =
            crate::runtime::budget::TurnBudget::for_role(crate::runtime::budget::TurnRole::Worker);
        b.max_provider_rounds = Some(3);
        assert!(!l.apply_to(&mut b));
        assert_eq!(b.max_provider_rounds, Some(3));
        let spent = WatchLease::remaining(&cfg, used.plus(100, 0)).unwrap_err();
        assert!(spent.starts_with("watch lease expired"), "{spent}");
        assert!(WatchLease::remaining(
            &cfg,
            WatchUsage {
                secs: 1,
                rounds: 40
            }
        )
        .is_err());
        let cfg = crate::config::WatchConfig {
            lease_cost_usd: Some(0.5),
            ..Default::default()
        };
        let mut b =
            crate::runtime::budget::TurnBudget::for_role(crate::runtime::budget::TurnRole::Worker);
        WatchLease::remaining(&cfg, WatchUsage::default())
            .unwrap()
            .apply_to(&mut b);
        assert_eq!(b.max_cost_usd, Some(0.5));
    }

    struct Says(&'static str, f64);
    #[async_trait::async_trait]
    impl Advisor for Says {
        async fn advise(&self, _request: serde_json::Value) -> Option<Advice> {
            Some(Advice {
                choice: self.0.into(),
                confidence: self.1,
            })
        }
    }

    fn dispatch<'a>(name: &'a str, task: &'a str, model: &'a str) -> Dispatch<'a> {
        Dispatch {
            request: req(Some(name), task),
            requested_model: None,
            resolved_model: model,
        }
    }

    #[tokio::test]
    async fn decide_refuses_polls_on_large_models_and_leases_them_on_small_ones() {
        let cfg = crate::config::WatchConfig::default();
        let big = "anthropic/claude-opus-4-7";
        let small = "muse/muse-wonder-1.0";
        let poll = dispatch(
            "t-lease-wait",
            "Block until /var/tmp/x/a.rc exists, then report it.",
            big,
        );
        let err = decide_dispatch("subagent_start", &poll, &cfg, None, None)
            .await
            .unwrap_err();
        assert!(
            err.starts_with("subagent_start refused (polling task):"),
            "{err}"
        );
        let on_small = Dispatch {
            resolved_model: small,
            ..poll
        };
        let ok = decide_dispatch("subagent_start", &on_small, &cfg, None, None)
            .await
            .unwrap();
        let (lease, used) = ok.watch.expect("a watch worker");
        assert_eq!(
            (lease.secs, lease.rounds, used),
            (7200, 40, WatchUsage::default())
        );
        // Plain work on a large model: allowed, no lease.
        let work = dispatch("implementer", "Implement the parser and its tests.", big);
        assert_eq!(
            decide_dispatch("subagent_start", &work, &cfg, None, None).await,
            Ok(Admitted { watch: None })
        );
        // The kill switch allows the poll; a small model still gets the lease.
        let off = crate::config::WatchConfig {
            admission: crate::config::WatchAdmission::Off,
            ..Default::default()
        };
        assert_eq!(
            decide_dispatch("subagent_start", &poll, &off, None, None).await,
            Ok(Admitted { watch: None })
        );
        assert!(
            decide_dispatch("subagent_start", &on_small, &off, None, None)
                .await
                .unwrap()
                .watch
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_ambiguous_dispatch_is_refused_only_on_confident_wait_advice() {
        let cfg = crate::config::WatchConfig::default();
        let d = dispatch(
            "reviewer",
            "Review the diff. Wait for CI and report when it is green.",
            "anthropic/claude-opus-4-7",
        );
        assert_eq!(
            decide_dispatch("subagent_start", &d, &cfg, None, None).await,
            Ok(Admitted { watch: None }),
            "no advisor: allowed"
        );
        let sure = Says("event_wait", 0.97);
        assert!(
            decide_dispatch("subagent_start", &d, &cfg, None, Some(&sure))
                .await
                .is_err()
        );
        let unsure = Says("bounded_watcher", 0.9);
        assert!(
            decide_dispatch("subagent_start", &d, &cfg, None, Some(&unsure))
                .await
                .is_ok()
        );
        let inspect = Says("inspect", 0.99);
        assert!(
            decide_dispatch("subagent_start", &d, &cfg, None, Some(&inspect))
                .await
                .is_ok()
        );
        let no_budget = crate::config::WatchConfig {
            advice_timeout_ms: 0,
            ..Default::default()
        };
        assert!(
            decide_dispatch("subagent_start", &d, &no_budget, None, Some(&sure))
                .await
                .is_ok(),
            "0 ms asks no advisor"
        );
    }

    #[tokio::test]
    async fn a_resumed_watch_worker_continues_its_lease_and_is_refused_once_it_is_spent() {
        let cfg = crate::config::WatchConfig::default();
        // New instructions that read as work do not end the lease.
        let d = dispatch(
            "lease-watcher",
            "Implement, fix, write and test the parser.",
            "muse/muse-wonder-1.0",
        );
        let used = WatchUsage {
            secs: 7000,
            rounds: 12,
        };
        let ok = decide_dispatch("subagent_resume", &d, &cfg, Some(used), None)
            .await
            .unwrap();
        let (lease, carried) = ok.watch.unwrap();
        assert_eq!((lease.secs, lease.rounds, carried), (200, 28, used));
        let spent = WatchUsage {
            secs: 7200,
            rounds: 12,
        };
        let d = dispatch(
            "lease-watcher",
            "Keep waiting for /var/tmp/x/a.rc.",
            "muse/muse-wonder-1.0",
        );
        let err = decide_dispatch("subagent_resume", &d, &cfg, Some(spent), None)
            .await
            .unwrap_err();
        assert!(
            err.starts_with("subagent_resume refused (watch lease expired:"),
            "{err}"
        );
        assert!(err.contains(r#""file":"/var/tmp/x/a.rc""#), "{err}");
    }

    /// Both admission lines reach the daemon log: its filter (RUST_LOG
    /// unset) keeps them at INFO under [`LOG_TARGET`] and drops the same
    /// level at the earlier bare `subagent_admission` target and at an
    /// unrelated one (the controls).
    #[tokio::test]
    async fn admission_lines_reach_the_daemon_log() {
        let cfg = crate::config::WatchConfig::default();
        let (log, guard) = crate::tools::test_helpers::DaemonLog::capture();
        let poll = dispatch(
            "t-lease-wait",
            "Block until /var/tmp/x/a.rc exists, then report it.",
            "anthropic/claude-opus-4-7",
        );
        assert!(decide_dispatch("subagent_start", &poll, &cfg, None, None)
            .await
            .is_err());
        let spent = WatchUsage {
            secs: 7200,
            rounds: 12,
        };
        let watcher = dispatch(
            "lease-watcher",
            "Keep waiting for /var/tmp/x/a.rc.",
            "muse/muse-wonder-1.0",
        );
        assert!(
            decide_dispatch("subagent_resume", &watcher, &cfg, Some(spent), None)
                .await
                .is_err()
        );
        tracing::info!(target: "subagent_admission", "control: the earlier target");
        tracing::info!(target: "some_dependency", "control: an unrelated target");
        drop(guard);
        let lines = log.find(LOG_TARGET, tracing::Level::INFO, "subagent admission");
        let actions: Vec<(&str, &str)> = lines
            .iter()
            .map(|l| (l.fields["entry"].as_str(), l.fields["action"].as_str()))
            .collect();
        assert_eq!(
            actions,
            [
                ("subagent_start", "refuse"),
                ("subagent_resume", "refuse_lease_expired")
            ],
            "{:?}",
            log.lines()
        );
        assert!(
            !log.passed("control:"),
            "the controls are dropped: {:?}",
            log.lines()
        );
    }
}
