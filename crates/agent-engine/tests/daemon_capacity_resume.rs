//! The capacity resume across REAL daemon processes: a session whose turn
//! stopped because no Anthropic account had capacity is resumed ONCE at the
//! reset after a `kill -9` (D1), a stop and start (D2), a real reload (same
//! pid, next generation), and while parked past its eviction age (D3); a
//! session the crash restore gives up drops it, and a client loading that
//! session later does not bring it back (D4).
//!
//! OPT-IN: these cases start real daemons, so they run only when the binary
//! is given `--real-daemons`, on a host set aside for daemon tests, one
//! daemon test at a time:
//!
//! ```text
//! cargo test -p synaps-engine --test daemon_capacity_resume -- --real-daemons
//! ```
//!
//! Without it the binary prints one skip line, starts no daemon and exits 0, so an ordinary `cargo test` (with or without filters) and CI
//! skip them. `--list` prints the case names and starts nothing; an unknown
//! flag is an error (exit 2) and starts nothing, and so is a flag that takes
//! a value (`--format`, `--test-threads`, `--color`, `--skip`) given none,
//! one that looks like a flag (`--real-daemons --format --bogus-flag`) or one
//! outside its set. Every flag is checked before any runtime, trap, scratch
//! dir or process exists.
//!
//! `harness = false`: this binary's `main` is the test AND, run as `daemon
//! --print-version` / `daemon --foreground` (the arguments the daemon's own
//! reload probe and exec use, reload.rs `probe_version` / `exec_self`), the
//! daemon under test: `agent_engine::daemon::run_foreground` with a scripted
//! credential broker installed for every turn (`turn_broker_testing`). No
//! production entry point or exec protocol changes. Test build only.
//! Daemon mode runs only with the marker an opted-in parent gives each
//! daemon it starts: `CAPRES_PARENT_TOKEN` = `capres-<parent pid>-<32 hex>`,
//! checked against the process's actual parent. A reload's exec keeps it
//! (the same process, its environment inherited). Started any other way
//! (`cargo test --test daemon_capacity_resume -- daemon --foreground`), it
//! refuses at once with exit 2 and touches nothing; the guard case pins
//! this. `daemon --print-version` only prints the version.
//!
//! Isolation (checked BEFORE the first request of each case):
//! - the child starts from `env_clear()` with only the variables listed in
//!   [`ALLOWED_ENV`]: a scratch HOME / base / runtime dir, the loopback
//!   provider (`SYNAPS_ANTHROPIC_BASE_URL`), and every proxy variable (upper
//!   and lower case) pointing at a loopback TRAP; `NO_PROXY` covers
//!   loopback only;
//! - the scratch base holds only `config` and `auth.json` (no extensions,
//!   plugins, skills or MCP config to discover), HOME is scratch, and the
//!   daemon's cwd is an empty work dir;
//! - every HTTP client the daemon builds outside tests is a `reqwest` client
//!   without `.no_proxy()` (the only `.no_proxy()` / `Proxy::` uses are in
//!   test modules), and nothing outside tests opens raw TCP or websocket
//!   connections, so any non-loopback HTTP request reaches the trap;
//! - the trap counts every connection at ACCEPT, before reading a byte (its
//!   own self-test takes a one-shot token set just before it connects),
//!   refuses it, and trips the run: the runner races each case against the
//!   trip (polled every 50 ms), drops the case at once and fails the run
//!   naming the case; no further case runs. The trap self-check case
//!   injects one connection mid-wait and requires the trip within 1 s;
//! - the teardown, after every case (passed, panicked or tripped) and before
//!   it can be reported ok: each process a case starts is in a process group
//!   of its own, and dropping its owner SIGKILLs the whole group and reaps
//!   the leader; every such group must then be empty (checked by its pgid);
//!   then each trap stops accepting and every connection still queued on its
//!   listener is accepted, counted and trips the run. The drain check (a
//!   connection after a case's last trap check fails that case) and the
//!   owner-drop check (a dropped owner empties its group, a background child
//!   of the leader included) pin both;
//! - credentials: the scratch `auth.json` holds one synthetic OAuth entry
//!   (it only makes the session an OAuth one); there is no account store.
//!   The turn's selection and its token (`broker_from_source` for a turn's
//!   refresh) are the scripted broker's, which never reads usage. Boot still
//!   reads the scratch config, `auth.json`, the empty extensions and sessions
//!   dirs and the run dir (crash record, reload state), all local files.
//!
//! One daemon at a time; the cases run in order. Unix only (kill -9, flock,
//! exec, process groups); on Linux the run makes itself the subreaper of the
//! orphans its processes leave, so the teardown reaps them itself.

#[cfg(not(unix))]
fn main() {}

#[cfg(unix)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("daemon") {
        child::run(&args[1..]);
    } else {
        parent::run(&args);
    }
}

/// The variables the child daemon may see (its env starts empty).
#[cfg(unix)]
const ALLOWED_ENV: &[&str] = &[
    "HOME",
    "PATH",
    "TMPDIR",
    "RUST_BACKTRACE",
    "SYNAPS_BASE_DIR",
    "SYNAPS_RUNTIME_DIR",
    "SYNAPS_DAEMON",
    "SYNAPS_ANTHROPIC_BASE_URL",
    "SYNAPS_PROCESS_SCOPES",
    "SYNAPS_DAEMON_PARK_GRACE_SECS",
    "SYNAPS_DAEMON_PARKED_EVICT_SECS",
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "CAPRES_RESET_MS",
    "CAPRES_JITTER_MS",
    "CAPRES_ACK_MS",
    "CAPRES_LOOK_MS",
    "CAPRES_PARENT_TOKEN",
];

/// Daemon mode's guard: the marker an opted-in parent gives each daemon it
/// starts, naming THIS process's actual parent (a reload's exec keeps both).
#[cfg(unix)]
fn opted_in() -> bool {
    let Ok(token) = std::env::var("CAPRES_PARENT_TOKEN") else {
        return false;
    };
    let Some((pid, hex)) = token.strip_prefix("capres-").and_then(|r| r.split_once('-')) else {
        return false;
    };
    // SAFETY: getppid has no preconditions and cannot fail.
    let parent = unsafe { libc::getppid() };
    pid.parse::<i32>().is_ok_and(|p| p == parent)
        && hex.len() == 32
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The control lines the child logs once (first selection, after boot): an
/// INFO line on a target outside Synaps's crates, which the daemon's filter
/// must drop, and an ERROR line on it, which it keeps.
#[cfg(unix)]
const CONTROL_INFO: &str = "capres harness control: info line, dropped by the daemon filter";
#[cfg(unix)]
const CONTROL_ERROR: &str = "capres harness control: error line, kept by the daemon filter";

// ── the daemon under test ───────────────────────────────────────────────

#[cfg(unix)]
mod child {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Once};
    use std::time::Duration;

    use agent_engine::auth::{
        AccessToken, Account, AccountFailure, AccountFailureKind, AccountSelector, BrokerError,
        CredentialBroker, CredentialRef, OAuthProviderId, PinnedToken, ProxyByteStream, ProxyRequest,
        ProxyResponse, SelectionFailure, SelectionFailureKind,
    };
    use agent_engine::daemon::{self, DaemonOpts};
    use serde_json::Value;

    fn env_ms(key: &str) -> Option<u64> {
        std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
    }

    pub fn run(args: &[String]) {
        if args.iter().any(|a| a == "--print-version") {
            println!(
                "{}",
                serde_json::to_string(&daemon::reload::PrintVersion::current()).expect("version json")
            );
            std::process::exit(0);
        }
        if !args.iter().any(|a| a == "--foreground") || !super::opted_in() || env_ms("CAPRES_RESET_MS").is_none() {
            eprintln!(
                "capres daemon mode refused: it runs only as a daemon started by this binary's \
                 `--real-daemons` run; nothing started"
            );
            std::process::exit(2);
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let code = rt.block_on(async {
            let reset_ms = env_ms("CAPRES_RESET_MS").expect("CAPRES_RESET_MS");
            let ms = |key: &str, default: u64| Duration::from_millis(env_ms(key).unwrap_or(default));
            let _timings = agent_engine::session::capacity_resume::testing::Timings::set(
                ms("CAPRES_JITTER_MS", 0),
                ms("CAPRES_ACK_MS", 5_000),
                ms("CAPRES_LOOK_MS", 1_000),
            );
            let _broker = agent_engine::runtime::turn_broker_testing::install_global(Arc::new(
                CoolUntilReset { reset_ms, selections: AtomicUsize::new(0) },
            ));
            let opts = DaemonOpts {
                socket: None,
                profile: None,
                idle_exit: None,
                allow_legacy_mcp: false,
                runtime_dir: None,
                factory: None,
            };
            match daemon::run_foreground(opts).await {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("capres daemon: {e}");
                    daemon::EXIT_REFUSED
                }
            }
        });
        std::process::exit(code);
    }

    /// Automatic selection: every account cools down until the reset; from
    /// the reset on an account is back.
    struct CoolUntilReset {
        reset_ms: u64,
        selections: AtomicUsize,
    }

    static CONTROL: Once = Once::new();

    #[async_trait::async_trait]
    impl CredentialBroker for CoolUntilReset {
        async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
            Err(BrokerError::Denied("legacy path must not be used".into()))
        }
        fn account_selector(&self, _p: OAuthProviderId) -> AccountSelector {
            AccountSelector::Auto
        }
        fn active_cooldown(&self, _cred: &CredentialRef) -> Option<u64> {
            (agent_engine::epoch_millis() < self.reset_ms).then_some(self.reset_ms)
        }
        async fn access_token_for(&self, _cred: &CredentialRef) -> Result<AccessToken, BrokerError> {
            Err(BrokerError::Denied("no explicit vend in this test".into()))
        }
        async fn access_token_pinned_for(
            &self,
            provider: OAuthProviderId,
            _model: Option<&str>,
        ) -> Result<PinnedToken, BrokerError> {
            CONTROL.call_once(|| {
                tracing::info!(target: "capres_harness", "{}", super::CONTROL_INFO);
                tracing::error!(target: "capres_harness", "{}", super::CONTROL_ERROR);
            });
            self.selections.fetch_add(1, Ordering::SeqCst);
            if agent_engine::epoch_millis() >= self.reset_ms {
                return Ok(PinnedToken {
                    credential: CredentialRef::new(provider, Account::parse("claude1").unwrap()),
                    token: AccessToken { token: "tok-scripted".into(), expires: u64::MAX },
                });
            }
            Err(BrokerError::NoAccountAvailable {
                provider: "anthropic".into(),
                reason: "scripted".into(),
                failure: Some(SelectionFailure {
                    kind: SelectionFailureKind::AllCoolingDown,
                    accounts: vec![AccountFailure {
                        account: "claude4".into(),
                        kind: AccountFailureKind::Exhausted,
                        http_status: None,
                        retry_after_s: None,
                        until_ms: Some(self.reset_ms),
                        cooling_down: true,
                    }],
                    earliest_reset_ms: Some(self.reset_ms),
                    usage_read_retry_ms: None,
                }),
            })
        }
        async fn proxy(&self, _r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }
        async fn proxy_stream(&self, _r: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }
        async fn anthropic_usage(&self) -> Result<Value, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }
        async fn capabilities(&self) -> Result<Vec<agent_engine::auth::ProviderStatus>, BrokerError> {
            Ok(vec![])
        }
    }
}

// ── the test ────────────────────────────────────────────────────────────

#[cfg(unix)]
mod parent {
    use std::collections::BTreeSet;
    use std::os::unix::process::CommandExt;
    use std::panic::AssertUnwindSafe;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use futures::FutureExt;
    use tokio::sync::oneshot;

    use agent_engine::daemon::registry::{self, DaemonPaths};
    use agent_engine::session::capacity_resume::armed_notice;
    use agent_engine::session::socket_transport::SocketTransport;
    use agent_engine::session::wire::*;
    use agent_engine::session::*;
    use axum::response::IntoResponse;
    use chrono::{DateTime, Utc};
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{ALLOWED_ENV, CONTROL_ERROR, CONTROL_INFO};

    const SSE_HI: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
        "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
        "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    const TASK: &str = "work that stops";
    const RESUME_PREFIX: &str = "resume: your last turn stopped at ";

    /// The log's message texts this test counts.
    const ARMED_LIVE: &str = "capacity resume: armed for the stop's reset";
    const ARMED_RESTORED: &str = "capacity resume: armed from the record";
    const CONSUMED: &str = "capacity resume: consumed";
    const DISPATCHED: &str = "capacity resume: dispatched";
    const EVICTION_HELD: &str = "capacity resume: the parked session is kept for its resume";
    const GIVEN_UP: &str = "capacity resume: dropped; the restore gave the session up";

    type Case = fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;

    pub fn run(args: &[String]) {
        // (name, raced against the trap's trip, case). The two trap checks
        // trip on purpose and reset the trip themselves.
        let cases: &[(&str, bool, Case)] = &[
            ("cli_check_a_flag_that_takes_a_value_refuses_a_missing_or_bad_one", true, || Box::pin(cli_check())),
            ("trap_self_check_an_unexpected_connection_trips_the_run_within_1s", false, || {
                Box::pin(trap_self_check())
            }),
            ("trap_drain_check_a_connection_left_at_teardown_fails_its_case", false, || {
                Box::pin(trap_drain_check())
            }),
            ("owner_drop_check_a_dropped_owner_kills_its_whole_process_group", true, || {
                Box::pin(owner_drop_check())
            }),
            ("guard_daemon_mode_without_its_parents_marker_refuses_and_starts_nothing", true, || {
                Box::pin(guard_direct())
            }),
            ("d1_a_kill_9_while_armed_resumes_once_in_the_next_daemon", true, || Box::pin(d1_kill_9())),
            ("d2_a_stop_and_start_resumes_once", true, || Box::pin(d2_stop_start())),
            ("d_reload_a_real_reload_resumes_once_in_the_same_process", true, || Box::pin(d_reload())),
            ("d3_a_parked_armed_session_outlives_its_eviction_and_resumes_once", true, || {
                Box::pin(d3_parked())
            }),
            ("d4_a_given_up_session_drops_its_resume_and_a_later_load_does_not_arm_it", true, || {
                Box::pin(d4_given_up())
            }),
        ];
        // Every flag is checked here, before any runtime, trap, scratch dir
        // or process exists.
        let opts = parse(args).unwrap_or_else(|e| {
            eprintln!(
                "error: {e}; known: --real-daemons, --list, --exact, --skip FILTER, and the libtest \
                 flags --nocapture --show-output --quiet -q --ignored --include-ignored, --test-threads N, \
                 --format pretty|terse|json|junit, --color auto|always|never; nothing started"
            );
            std::process::exit(2);
        });
        if opts.list {
            for (name, _, _) in cases {
                println!("{name}: test");
            }
            return;
        }
        if !opts.real {
            println!(
                "capres: skipped: {} real-daemon cases run only with `cargo test -p synaps-engine --test \
                 daemon_capacity_resume -- --real-daemons`; 0 daemons started",
                cases.len()
            );
            return;
        }
        // Orphans of the processes this run starts (a killed shell's
        // background child) come to this process, so the teardown reaps
        // them itself instead of relying on the host's init.
        #[cfg(target_os = "linux")]
        {
            // SAFETY: PR_SET_CHILD_SUBREAPER only marks this process as the
            // reaper of its orphaned descendants.
            let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1 as libc::c_ulong, 0, 0, 0) };
            if rc != 0 {
                println!(
                    "note: could not become the subreaper ({}); orphans are left to the host's init",
                    std::io::Error::last_os_error()
                );
            }
        }
        let picked: Vec<_> = cases.iter().filter(|(name, _, _)| opts.picks(name)).collect();
        println!("\nrunning {} tests", picked.len());
        let (mut passed, mut failed) = (0, Vec::new());
        for (name, raced, case) in picked {
            println!("test {name} ... started");
            let started = Instant::now();
            let end = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(run_case(*raced, *case));
            let secs = started.elapsed().as_secs_f64();
            let groups = match &end.groups {
                Ok(n) => format!("its {n} process group(s) are empty"),
                Err(e) => format!("NOT every process it started is gone: {e}"),
            };
            // A trip a panic overtook, or one only the teardown's drain saw,
            // is still this case's trip.
            if let Some(detail) = trip_detail() {
                println!(
                    "test {name} ... FAILED ({secs:.1}s): the egress trap tripped: {detail}; the case was \
                     stopped and {groups}; no further case runs"
                );
                failed.push(*name);
                report(passed, &failed, cases.len());
            }
            match (&end.outcome, &end.groups) {
                (Ok(()), Ok(_)) => {
                    passed += 1;
                    println!("test {name} ... ok ({secs:.1}s)");
                }
                (Ok(()), Err(_)) => {
                    println!("test {name} ... FAILED ({secs:.1}s): {groups}");
                    failed.push(*name);
                }
                (Err(_), _) => {
                    println!("test {name} ... FAILED ({secs:.1}s); {groups}");
                    failed.push(*name);
                }
            }
        }
        report(passed, &failed, cases.len());
    }

    /// The parent's command line (what cargo passes after `--`).
    #[derive(Debug, Default)]
    struct Opts {
        real: bool,
        list: bool,
        exact: bool,
        filters: Vec<String>,
        skips: Vec<String>,
    }

    impl Opts {
        /// Whether case `name` runs: it matches a filter (when there is
        /// one) and no `--skip`; `--exact` makes both whole-name matches.
        fn picks(&self, name: &str) -> bool {
            let hit = |f: &String| if self.exact { name == f } else { name.contains(f.as_str()) };
            (self.filters.is_empty() || self.filters.iter().any(hit)) && !self.skips.iter().any(hit)
        }
    }

    /// Every flag, checked before anything starts. An unknown flag is an
    /// error, and so is a flag that takes a value given none, an
    /// option-looking one (it would swallow the next flag: `--format
    /// --bogus-flag`) or one outside its set. `Err` names it.
    fn parse(args: &[String]) -> Result<Opts, String> {
        let mut o = Opts::default();
        let mut it = args.iter().map(String::as_str);
        while let Some(a) = it.next() {
            let (flag, inline) = match a.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f, Some(v)),
                _ => (a, None),
            };
            match flag {
                "--real-daemons" if inline.is_none() => o.real = true,
                "--list" if inline.is_none() => o.list = true,
                "--exact" if inline.is_none() => o.exact = true,
                // libtest flags a runner may pass: no effect here.
                "--nocapture" | "--show-output" | "--quiet" | "-q" | "--ignored" | "--include-ignored"
                    if inline.is_none() => {}
                "--test-threads" | "--format" | "--color" | "--skip" => {
                    let value = inline
                        .or_else(|| it.next())
                        .filter(|v| !v.is_empty() && !v.starts_with('-'))
                        .ok_or_else(|| format!("{flag} takes a value, and {a:?} has none (or one that looks like a flag)"))?;
                    let valid = match flag {
                        "--test-threads" => value.parse::<usize>().is_ok_and(|n| n > 0),
                        "--format" => ["pretty", "terse", "json", "junit"].contains(&value),
                        "--color" => ["auto", "always", "never"].contains(&value),
                        _ => true,
                    };
                    if !valid {
                        return Err(format!("{flag} {value:?} is not a value it takes"));
                    }
                    if flag == "--skip" {
                        o.skips.push(value.to_string());
                    }
                }
                f if f.starts_with('-') => return Err(format!("unknown flag {a:?}")),
                f => o.filters.push(f.to_string()),
            }
        }
        Ok(o)
    }

    /// How a case ended, its teardown included.
    struct CaseEnd {
        /// The case's own outcome: ok, or its panic.
        outcome: Result<(), Box<dyn std::any::Any + Send>>,
        /// Every process group the case started is empty (`Ok`: how many).
        groups: Result<usize, String>,
        /// Trap connections only the teardown's drain found.
        drained: usize,
    }

    /// One case and its teardown, inside the case's runtime: the case runs
    /// (raced against the trap's trip when `raced`) and its future is
    /// dropped, which kills each process group it owns and reaps the
    /// leader; then every group it started must be empty; then each of its
    /// traps stops accepting and every connection still in its backlog is
    /// counted and trips the run. Only after that can the case be reported
    /// ok, and only then is the runtime (and with it the traps) dropped.
    async fn run_case(raced: bool, case: Case) -> CaseEnd {
        let ran = AssertUnwindSafe(case()).catch_unwind();
        let outcome = if raced {
            tokio::select! {
                outcome = ran => outcome,
                // The trip is read from TRIP after the teardown.
                _ = tripped() => Ok(()),
            }
        } else {
            ran.await
        };
        let groups = groups_gone().await;
        let drained = drain_traps().await;
        CaseEnd { outcome, groups, drained }
    }

    /// The summary line; exits 101 when a case failed.
    fn report(passed: usize, failed: &[&str], total: usize) {
        let result = if failed.is_empty() { "ok" } else { "FAILED" };
        println!(
            "\ntest result: {result}. {passed} passed; {} failed; 0 ignored; 0 measured; {} filtered out\n",
            failed.len(),
            total - passed - failed.len()
        );
        if !failed.is_empty() {
            println!("failures:\n    {}", failed.join("\n    "));
            std::process::exit(101);
        }
    }

    // ── the trap's tripwire ─────────────────────────────────────────────

    /// The first unexpected trap connection's detail, for the whole run.
    static TRIP: Mutex<Option<String>> = Mutex::new(None);

    fn trip(detail: String) {
        TRIP.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert(detail);
    }

    fn trip_detail() -> Option<String> {
        TRIP.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    async fn tripped() -> String {
        loop {
            if let Some(detail) = trip_detail() {
                return detail;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Run `case` until it ends or the trap trips. On a trip the case's
    /// future is dropped at once (each process it owns is killed with its
    /// whole group and reaped, see [`Owned`]) and the trip's detail is
    /// returned.
    async fn race(case: impl std::future::Future<Output = ()>) -> Result<(), String> {
        tokio::select! {
            () = case => Ok(()),
            detail = tripped() => Err(detail),
        }
    }

    // ── the processes a case starts ─────────────────────────────────────

    /// The process groups started since the last teardown took them.
    static GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

    /// A process this test started, in a process group of its own (its
    /// pgid is its pid), owned until it is reaped. Dropping it (a case that
    /// ended, a case the trap's trip dropped, a panic) SIGKILLs its whole
    /// group and reaps the leader; a daemon's reload probe and anything
    /// else it starts is in that group.
    struct Owned {
        child: Child,
        reaped: Option<ExitStatus>,
    }

    impl Owned {
        fn spawn(mut cmd: Command, what: &str) -> Self {
            cmd.process_group(0);
            let child = cmd.spawn().unwrap_or_else(|e| panic!("start {what}: {e}"));
            GROUPS.lock().unwrap_or_else(|p| p.into_inner()).push(child.id() as i32);
            Self { child, reaped: None }
        }

        fn pid(&self) -> u32 {
            self.child.id()
        }

        /// Whether the leader has exited. When it has, the rest of its group
        /// is killed and the leader reaped (it is left unreaped until then,
        /// so its pgid can't name another group).
        fn exited(&mut self) -> bool {
            if self.reaped.is_some() {
                return true;
            }
            // SAFETY: an all-zero siginfo_t is valid; waitid only writes it.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid on this process's own child; WNOWAIT leaves it
            // unreaped, WNOHANG returns at once.
            let waited = loop {
                let rc = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        self.child.id() as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                if rc == 0 {
                    break Ok(());
                }
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    break Err(e);
                }
            };
            if let Err(e) = waited {
                panic!("waitid {}: {e}", self.child.id());
            }
            // SAFETY: waitid filled a SIGCHLD-shaped siginfo_t (or left it zero).
            if unsafe { info.si_pid() } == 0 {
                return false;
            }
            if let Err(e) = self.kill_and_reap() {
                panic!("process group {}: {e}", self.child.id());
            }
            true
        }

        /// SIGKILL the whole group while the leader is unreaped (its pgid
        /// still names this group), then reap the leader. A kill that fails
        /// (other than ESRCH) is reported without waiting, so a leader that
        /// may still run can't hang the teardown; its group check then
        /// reports the group within its deadline.
        fn kill_and_reap(&mut self) -> Result<ExitStatus, Cleanup> {
            if let Some(s) = self.reaped {
                return Ok(s);
            }
            // SAFETY: the group this test made; its leader is not reaped yet.
            if unsafe { libc::killpg(self.child.id() as i32, libc::SIGKILL) } != 0 {
                let err = std::io::Error::last_os_error();
                // ESRCH: no process in the group can take a signal any more
                // (an exited leader alone in it), so the wait below returns
                // at once.
                if err.raw_os_error() != Some(libc::ESRCH) {
                    self.reaped = self.child.try_wait().ok().flatten();
                    return Err(Cleanup::KillFailed { err, reaped: self.reaped.is_some() });
                }
            }
            // Child::wait retries on EINTR itself.
            let s = self.child.wait().map_err(Cleanup::ReapFailed)?;
            self.reaped = Some(s);
            Ok(s)
        }
    }

    /// Why an owned process's group was not killed and its leader reaped.
    #[derive(Debug)]
    enum Cleanup {
        /// killpg failed (not ESRCH); the leader was not waited for.
        KillFailed { err: std::io::Error, reaped: bool },
        /// The kill was sent; waiting for the leader failed.
        ReapFailed(std::io::Error),
    }

    impl std::fmt::Display for Cleanup {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Cleanup::KillFailed { err, reaped: true } => {
                    write!(f, "kill failed: {err}; its leader had already exited and is reaped")
                }
                Cleanup::KillFailed { err, reaped: false } => write!(f, "kill failed: {err}; its leader is not reaped"),
                Cleanup::ReapFailed(err) => write!(f, "kill sent; reap failed: {err}"),
            }
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            // Nothing is printed on success; only what is known on a failure.
            if let Err(e) = self.kill_and_reap() {
                println!("    process group {}: {e}", self.child.id());
            }
        }
    }

    /// Whether any process is still in group `pgid`.
    fn group_alive(pgid: i32) -> bool {
        // SAFETY: signal 0 only asks whether the group has a process.
        let rc = unsafe { libc::kill(-pgid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    /// Reap every exited child of this process in group `pgid` (an orphan
    /// it took in as the subreaper); each leader is reaped by its owner.
    fn reap_orphans(pgid: i32) {
        let mut status = 0;
        loop {
            // SAFETY: waitpid on this process's own children, without blocking.
            let pid = unsafe { libc::waitpid(-pgid, &mut status, libc::WNOHANG) };
            if pid > 0 {
                continue;
            }
            if pid < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
    }

    /// Every group in `groups` is empty within 5 s: `Ok(how many)`, or the
    /// groups that still have a process.
    async fn groups_empty(groups: &[i32]) -> Result<usize, String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            groups.iter().for_each(|g| reap_orphans(*g));
            let alive: Vec<i32> = groups.iter().copied().filter(|g| group_alive(*g)).collect();
            if alive.is_empty() {
                return Ok(groups.len());
            }
            if Instant::now() >= deadline {
                return Err(format!("process group(s) {alive:?} still have a process 5 s after their owners were dropped"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The teardown's check: every group started since the last one is
    /// empty (their owners are dropped by now).
    async fn groups_gone() -> Result<usize, String> {
        let groups = std::mem::take(&mut *GROUPS.lock().unwrap_or_else(|p| p.into_inner()));
        groups_empty(&groups).await
    }

    /// This run's opt-in marker for the daemons it starts (see the header).
    fn token() -> &'static str {
        static TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        TOKEN.get_or_init(|| {
            use std::hash::{BuildHasher, Hasher};
            let word = |salt: u64| {
                let mut h = std::collections::hash_map::RandomState::new().build_hasher();
                h.write_u64(salt);
                h.finish()
            };
            format!("capres-{}-{:016x}{:016x}", std::process::id(), word(1), word(2))
        })
    }

    /// `key`'s value in the environment process `pid` was started (or last
    /// exec'd) with.
    fn environ_value(pid: u32, key: &str) -> Option<String> {
        let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        environ.split(|b| *b == 0).find_map(|kv| {
            let kv = String::from_utf8_lossy(kv);
            kv.strip_prefix(key).and_then(|r| r.strip_prefix('=')).map(str::to_string)
        })
    }

    // ── the cases ───────────────────────────────────────────────────────

    /// The parent's flags: a flag that takes a value refuses a missing one,
    /// an option-looking one (so `--format --bogus-flag` can't swallow a
    /// flag) and one outside its set, in the `=` form too; `--skip` and
    /// `--exact` pick as libtest's do. (The binary's own exit 2 on such a
    /// command line is the g1pro argument check.)
    async fn cli_check() {
        let parsed = |args: &[&str]| parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        let refused: &[&[&str]] = &[
            &["--real-daemons", "--format", "--bogus-flag"],
            &["--real-daemons", "--format"],
            &["--format="],
            &["--format", "yaml"],
            &["--format=-q"],
            &["--test-threads", "0"],
            &["--test-threads", "two"],
            &["--test-threads=-1"],
            &["--color", "--list"],
            &["--color=sometimes"],
            &["--skip"],
            &["--skip", "--real-daemons"],
            &["--list=yes"],
            &["--real-daemons=1"],
            &["--bogus-flag"],
            &["-x"],
        ];
        for args in refused {
            let e = parsed(args).expect_err(&format!("{args:?} must be refused"));
            println!("    refused {args:?}: {e}");
        }
        let o = parsed(&[
            "--real-daemons", "--format", "terse", "--test-threads=1", "--color", "never", "--nocapture", "--skip",
            "d3_", "--skip=d4_", "d",
        ])
        .expect("a valid command line");
        assert!(o.real && !o.list && !o.exact, "{o:?}");
        assert_eq!((o.filters.as_slice(), o.skips.as_slice()), (&["d".to_string()][..], &["d3_".to_string(), "d4_".to_string()][..]));
        assert!(o.picks("d1_a_kill_9") && !o.picks("d3_parked") && !o.picks("d4_given_up"), "{o:?}");
        let o = parsed(&["--exact", "d1"]).expect("valid");
        assert!(o.picks("d1") && !o.picks("d1_a_kill_9"), "{o:?}");
        let o = parsed(&["--list"]).expect("valid");
        assert!(o.list && !o.real && o.picks("anything"), "{o:?}");
    }

    /// The trap's tripwire, through the runner's own race: one unexpected
    /// connection to the trap in the middle of a case's wait (a raw connect,
    /// no byte sent) trips the run within 1 s; the case is dropped, and the
    /// daemon's whole process group is killed and reaped.
    async fn trap_self_check() {
        let lab = Lab::new(Duration::from_secs(60), &[]).await;
        let a = lab.spawn().await;
        lab.preflight(&a).await;
        let pid = a.proc.pid();
        let addr = lab.trap.addr;
        let injected: Arc<Mutex<Option<Instant>>> = Arc::default();
        let at = Arc::clone(&injected);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            *at.lock().unwrap() = Some(Instant::now());
            let _conn = tokio::net::TcpStream::connect(addr).await.expect("connect to the trap");
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let outcome = race(async move {
            let _held = a;
            tokio::time::sleep(Duration::from_secs(20)).await;
        })
        .await;
        let tripped_at = Instant::now();
        let detail = outcome.expect_err("the injected connection never tripped the run");
        let after = tripped_at.duration_since(injected.lock().unwrap().expect("injected"));
        assert!(after <= Duration::from_secs(1), "tripped {after:?} after the connection");
        assert!(detail.contains("at accept"), "{detail}");
        assert_eq!(lab.trap.hits(), 1, "{:?}", lab.trap.seen());
        let gone = groups_empty(&[pid as i32]).await;
        assert_eq!(gone, Ok(1), "the tripped case's daemon group {pid} is empty");
        *TRIP.lock().unwrap_or_else(|p| p.into_inner()) = None;
        println!(
            "    trap self-check: tripped {after:?} after the injected connection ({detail}); daemon group {pid} empty"
        );
    }

    /// The teardown's drain: a connection the trap never accepted (here the
    /// case stops the trap's accept loop after its last trap check, connects
    /// once and returns ok) still fails that case: after its daemon group is
    /// killed and reaped, the runner's teardown accepts the connection from
    /// the listener's backlog, counts it and trips.
    async fn trap_drain_check() {
        let end = run_case(false, || {
            Box::pin(async {
                let lab = Lab::new(Duration::from_secs(60), &[]).await;
                let a = lab.spawn().await;
                lab.preflight(&a).await;
                assert_eq!(lab.trap.hits(), 0, "{:?}", lab.trap.seen());
                lab.trap.stop_accepting().await;
                let _late = std::net::TcpStream::connect(lab.trap.addr).expect("connect to the trap");
                drop(a);
            })
        })
        .await;
        let detail = trip_detail();
        *TRIP.lock().unwrap_or_else(|p| p.into_inner()) = None;
        assert!(end.outcome.is_ok(), "the inner case itself passed");
        assert_eq!(end.drained, 1, "the teardown drained the late connection; trip: {detail:?}");
        let detail = detail.expect("the drained connection tripped the run");
        assert!(detail.contains("at teardown"), "{detail}");
        assert_eq!(end.groups, Ok(1), "the inner case's daemon group is empty");
        println!("    trap drain check: {detail}; the inner case's 1 process group is empty");
    }

    /// The owner: an [`Owned`] dropped while its case waits (as a trip drops
    /// a case) kills its whole process group, a background child of the
    /// leader included, and reaps the leader; the group is then empty.
    async fn owner_drop_check() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30 & echo $!; exec sleep 30"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut owned = Owned::spawn(cmd, "a shell with a background child");
        let pgid = owned.pid() as i32;
        let mut line = String::new();
        let mut out = std::io::BufReader::new(owned.child.stdout.take().expect("stdout"));
        std::io::BufRead::read_line(&mut out, &mut line).expect("the background child's pid");
        let bg: i32 = line.trim().parse().expect("a pid");
        // SAFETY: getpgid only reads a process's group.
        let (leader_group, bg_group) = unsafe { (libc::getpgid(pgid), libc::getpgid(bg)) };
        assert_eq!((leader_group, bg_group), (pgid, pgid), "the leader and its background child share its group");
        let dropped = tokio::select! {
            () = async move {
                let _held = owned;
                tokio::time::sleep(Duration::from_secs(30)).await;
            } => false,
            () = tokio::time::sleep(Duration::from_millis(300)) => true,
        };
        assert!(dropped, "the owner was dropped mid-wait");
        assert_eq!(groups_empty(&[pgid]).await, Ok(1), "the dropped owner's group {pgid} is empty");
        // SAFETY: as above.
        assert_eq!(unsafe { libc::getpgid(bg) }, -1, "the background child {bg} is gone");
        println!("    owner drop: group {pgid} (its leader and background child {bg}) is empty after the drop");
    }

    /// The guard: daemon mode started without the opted-in parent's marker
    /// (as `cargo test --test daemon_capacity_resume -- daemon --foreground`
    /// would start it), or with a marker naming another parent, refuses at
    /// once with exit 2 and starts nothing: the run dir stays empty and no
    /// log is written.
    async fn guard_direct() {
        let lab = Lab::new(Duration::from_secs(60), &[]).await;
        let other = format!("capres-1-{}", "0f".repeat(16));
        for (what, marker) in [("no marker", None), ("another parent's marker", Some(other))] {
            let mut env = lab.child_env();
            env.retain(|(k, _)| k != "CAPRES_PARENT_TOKEN");
            env.extend(marker.map(|m| ("CAPRES_PARENT_TOKEN".to_string(), m)));
            let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
            cmd.args(["daemon", "--foreground"])
                .env_clear()
                .envs(env)
                .current_dir(lab.root.path().join("work"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            // Owned: a panic or a trip here kills its group and reaps it.
            let mut child = Owned::spawn(cmd, "daemon mode directly");
            let deadline = Instant::now() + Duration::from_secs(10);
            while !child.exited() {
                assert!(Instant::now() < deadline, "{what}: daemon mode did not refuse at once");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let status = child.reaped.expect("reaped");
            let mut err = String::new();
            std::io::Read::read_to_string(&mut child.child.stderr.take().expect("stderr"), &mut err).unwrap();
            assert_eq!(status.code(), Some(2), "{what}: {err}");
            assert!(err.contains("capres daemon mode refused"), "{what}: {err}");
            println!("    guard ({what}): exit 2: {}", err.trim());
        }
        let run: Vec<_> =
            std::fs::read_dir(lab.run_dir()).unwrap().filter_map(|e| e.ok().map(|e| e.file_name())).collect();
        assert!(run.is_empty(), "nothing in the run dir: {run:?}");
        assert!(lab.log_text().is_empty(), "no daemon log");
        assert_eq!(lab.trap.hits(), 0, "{:?}", lab.trap.seen());
        assert_eq!(lab.hits(), 0);
    }

    /// D1: armed, then `kill -9` (no shutdown path); the next daemon's crash
    /// restore arms it from the record and resumes it once at the reset.
    async fn d1_kill_9() {
        let lab = Lab::new(Duration::from_secs(20), &[]).await;
        let mut a = lab.spawn().await;
        lab.preflight(&a).await;
        let (_t, id) = lab.stopped_session(&a).await;
        lab.wait_log(ARMED_LIVE, 1).await;
        a.kill_9();
        assert_eq!(lab.hits(), 0, "nothing sent before the reset");

        let b = lab.spawn().await;
        lab.wait_log(ARMED_RESTORED, 1).await;
        lab.expect_one_resume(&id).await;
        lab.stop(b).await;
        lab.expect_log(ARMED_LIVE, 1);
        lab.expect_log(ARMED_RESTORED, 1);
        lab.end_checks();
    }

    /// D2: armed, then a clean stop; the next start restores it from the stop
    /// record and resumes it once.
    async fn d2_stop_start() {
        let lab = Lab::new(Duration::from_secs(20), &[]).await;
        let a = lab.spawn().await;
        lab.preflight(&a).await;
        let (t, id) = lab.stopped_session(&a).await;
        drop(t);
        lab.stop(a).await;
        assert!(lab.record_text().contains("\"capacity_stop\""), "the stop record carries it");

        let c = lab.spawn().await;
        lab.wait_log(ARMED_RESTORED, 1).await;
        lab.expect_one_resume(&id).await;
        lab.stop(c).await;
        lab.expect_log(ARMED_RESTORED, 1);
        lab.end_checks();
    }

    /// A real reload: prepare, checkpoint, `exec` of this binary as `daemon
    /// --foreground` (the same pid, the lock adopted), the reload state's
    /// rehydrate arms the resume, and it fires once.
    async fn d_reload() {
        let lab = Lab::new(Duration::from_secs(20), &[]).await;
        let a = lab.spawn().await;
        lab.preflight(&a).await;
        let pid = SocketTransport::ping(&a.paths.sock).await.expect("ping").pid;
        let before = registry::read_daemon_json(&a.paths).expect("daemon.json").generation;
        let (t, id) = lab.stopped_session(&a).await;
        drop(t);
        let said = SocketTransport::reload(&a.paths.sock, false, Some(5), None)
            .await
            .expect("the reload is accepted");
        println!("    reload: generation {before} -> bye says {said}");
        wait_until("the new image publishes the next generation", Duration::from_secs(60), || {
            registry::read_daemon_json(&a.paths).is_some_and(|i| i.generation == before + 1)
        })
        .await;
        let pong = wait_ping(&a.paths).await;
        assert_eq!(pong.pid, pid, "a reload execs in place: the same pid");
        assert_eq!(registry::read_daemon_json(&a.paths).map(|i| i.pid), Some(pid));
        assert_eq!(
            environ_value(pid, "CAPRES_PARENT_TOKEN").as_deref(),
            Some(token()),
            "the exec kept the opt-in marker"
        );
        lab.wait_log(ARMED_RESTORED, 1).await;
        lab.expect_one_resume(&id).await;
        let pong = SocketTransport::ping(&a.paths.sock).await.expect("ping");
        assert_eq!(pong.pid, pid);
        lab.stop(a).await;
        lab.expect_log(ARMED_LIVE, 1);
        lab.expect_log(ARMED_RESTORED, 1);
        lab.end_checks();
    }

    /// D3: armed and detached, the session parks after 1 s and reaches its
    /// eviction age (2 s) long before the reset: it is held, not evicted, and
    /// resumes once. Control: after that turn, with no stop armed, the same
    /// session parks and IS evicted.
    async fn d3_parked() {
        let lab = Lab::new(
            Duration::from_secs(18),
            &[("SYNAPS_DAEMON_PARK_GRACE_SECS", "1"), ("SYNAPS_DAEMON_PARKED_EVICT_SECS", "2")],
        )
        .await;
        let a = lab.spawn().await;
        lab.preflight(&a).await;
        let (t, id) = lab.stopped_session(&a).await;
        t.detach().await;
        drop(t);
        lab.wait_log(EVICTION_HELD, 1).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let listed = lab.listed(&a, &id).await;
        assert_eq!(
            listed.as_ref().map(|m| m.lifecycle),
            Some(SessionLifecycle::Parked),
            "parked and kept past its eviction age: {listed:?}"
        );
        lab.expect_one_resume(&id).await;
        // Control: nothing holds it now; it parks again and is evicted.
        let deadline = Instant::now() + Duration::from_secs(30);
        while lab.listed(&a, &id).await.is_some() {
            assert!(Instant::now() < deadline, "the session was never evicted after its resume");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        lab.stop(a).await;
        lab.end_checks();
    }

    /// D4: armed, `kill -9`, and the session's journal is locked by another
    /// process while the next daemon restores: its create fails, the restore
    /// gives it up and drops the resume (the record is rewritten without
    /// it). A client then loads the session after the reset: nothing is
    /// armed, nothing is sent.
    async fn d4_given_up() {
        let lab = Lab::new(Duration::from_secs(20), &[]).await;
        let mut a = lab.spawn().await;
        lab.preflight(&a).await;
        let (_t, id) = lab.stopped_session(&a).await;
        let journal = lab.journal_of(&id);
        a.kill_9();

        let lock = hold_lock(&lab.base().join("sessions").join(format!("{journal}.lock")));
        let b = lab.spawn().await;
        lab.wait_log(GIVEN_UP, 1).await;
        wait_until("the record drops the resume", Duration::from_secs(10), || {
            let text = lab.record_text();
            text.contains(&id) && !text.contains("\"capacity_stop\"")
        })
        .await;
        drop(lock);
        lab.past_reset(Duration::from_secs(2)).await;
        assert_eq!(lab.hits(), 0, "a given-up session is not resumed");

        let config = SessionConfig { continue_session: Some(Some(journal.clone())), ..lab.session_config() };
        let (_loaded, _) = attach(&b.paths, Attach::Create { config, mode: AttachMode::Mirror }).await;
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert_eq!(lab.hits(), 0, "loading it later arms nothing: {:#?}", lab.bodies());
        lab.stop(b).await;
        lab.expect_log(ARMED_RESTORED, 0);
        lab.expect_log(GIVEN_UP, 1);
        lab.expect_log(DISPATCHED, 0);
        lab.end_checks_with(0);
    }

    // ── the lab: scratch dirs, the provider stub, the trap ──────────────

    struct Lab {
        root: tempfile::TempDir,
        url: String,
        hits: Arc<AtomicUsize>,
        bodies: Arc<Mutex<Vec<Value>>>,
        /// When each provider request arrived.
        arrivals: Arc<Mutex<Vec<DateTime<Utc>>>>,
        trap: Trap,
        reset_ms: u64,
        extra: Vec<(String, String)>,
    }

    /// A daemon under test; dropping it kills its group and reaps it.
    struct Daemon {
        proc: Owned,
        paths: DaemonPaths,
    }

    impl Daemon {
        /// SIGKILL to its whole group: no shutdown path runs.
        fn kill_9(&mut self) {
            if let Err(e) = self.proc.kill_and_reap() {
                panic!("kill -9 the daemon's group {}: {e}", self.proc.pid());
            }
        }
    }

    impl Lab {
        async fn new(reset_in: Duration, extra: &[(&str, &str)]) -> Self {
            let root = tempfile::TempDir::new().expect("scratch root");
            let base = root.path().join("home").join(".synaps-cli");
            std::fs::create_dir_all(base.join("run")).unwrap();
            std::fs::create_dir_all(root.path().join("work")).unwrap();
            std::fs::create_dir_all(root.path().join("tmp")).unwrap();
            std::fs::write(base.join("config"), "capacity_wait = off\n").unwrap();
            std::fs::write(
                base.join("auth.json"),
                "{\"anthropic\": {\"type\": \"oauth\", \"refresh\": \"r\", \"access\": \"synthetic-token\", \"expires\": 9999999999999}}",
            )
            .unwrap();
            let (url, hits, bodies, arrivals) = provider_stub().await;
            let trap = Trap::start().await;
            let reset_ms = agent_engine::epoch_millis() + reset_in.as_millis() as u64;
            Self {
                root,
                url,
                hits,
                bodies,
                arrivals,
                trap,
                reset_ms,
                extra: extra.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            }
        }

        fn base(&self) -> PathBuf {
            self.root.path().join("home").join(".synaps-cli")
        }

        fn run_dir(&self) -> PathBuf {
            self.base().join("run")
        }

        fn resume_at(&self) -> DateTime<Utc> {
            DateTime::<Utc>::from_timestamp_millis(self.reset_ms as i64).unwrap()
        }

        fn child_env(&self) -> Vec<(String, String)> {
            let trap = self.trap.url.clone();
            let mut env: Vec<(String, String)> = vec![
                ("HOME".into(), self.root.path().join("home").display().to_string()),
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("TMPDIR".into(), self.root.path().join("tmp").display().to_string()),
                ("RUST_BACKTRACE".into(), "1".into()),
                ("SYNAPS_BASE_DIR".into(), self.base().display().to_string()),
                ("SYNAPS_RUNTIME_DIR".into(), self.run_dir().display().to_string()),
                ("SYNAPS_DAEMON".into(), "1".into()),
                ("SYNAPS_ANTHROPIC_BASE_URL".into(), self.url.clone()),
                ("SYNAPS_PROCESS_SCOPES".into(), "off".into()),
                ("NO_PROXY".into(), "127.0.0.1,localhost".into()),
                ("no_proxy".into(), "127.0.0.1,localhost".into()),
                ("CAPRES_RESET_MS".into(), self.reset_ms.to_string()),
                ("CAPRES_JITTER_MS".into(), "1000".into()),
                ("CAPRES_ACK_MS".into(), "5000".into()),
                ("CAPRES_LOOK_MS".into(), "1000".into()),
                ("CAPRES_PARENT_TOKEN".into(), token().into()),
            ];
            for k in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
                env.push((k.into(), trap.clone()));
            }
            if !self.extra.iter().any(|(k, _)| k == "SYNAPS_DAEMON_PARK_GRACE_SECS") {
                env.push(("SYNAPS_DAEMON_PARK_GRACE_SECS".into(), "never".into()));
            }
            env.extend(self.extra.iter().cloned());
            env
        }

        async fn spawn(&self) -> Daemon {
            let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
            cmd.args(["daemon", "--foreground"])
                .env_clear()
                .envs(self.child_env())
                .current_dir(self.root.path().join("work"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit());
            let d = Daemon {
                proc: Owned::spawn(cmd, "the daemon"),
                paths: registry::daemon_paths_in(&self.run_dir(), None),
            };
            wait_ping(&d.paths).await;
            d
        }

        /// The isolation checks, before the first request: the trap counts
        /// and refuses (a self-test the trap files apart), nothing reached it
        /// during the boot, the child's environment is exactly the allowed
        /// one, and the scratch store holds no account.
        async fn preflight(&self, d: &Daemon) {
            self.trap.self_test().await;
            assert_eq!(self.trap.hits(), 0, "the boot reached the trap: {:?}", self.trap.seen());
            let environ = std::fs::read(format!("/proc/{}/environ", d.proc.pid())).expect("the child's environ");
            let keys: BTreeSet<String> = environ
                .split(|b| *b == 0)
                .filter(|kv| !kv.is_empty())
                .map(|kv| {
                    let kv = String::from_utf8_lossy(kv);
                    kv.split('=').next().unwrap_or_default().to_string()
                })
                .collect();
            let allowed: BTreeSet<String> = ALLOWED_ENV.iter().map(|k| k.to_string()).collect();
            let extra: Vec<&String> = keys.difference(&allowed).collect();
            assert!(extra.is_empty(), "the child sees variables outside the allowed set: {extra:?}");
            for k in ["HTTP_PROXY", "https_proxy", "ALL_PROXY", "SYNAPS_ANTHROPIC_BASE_URL", "HOME"] {
                assert!(keys.contains(k), "the child lacks {k}");
            }
            // Named accounts are `<provider>@<label>` keys of auth.json
            // (storage.rs `load_provider_auth`): the store holds only the
            // one synthetic default entry, so there is no stored account.
            let auth: Value =
                serde_json::from_str(&std::fs::read_to_string(self.base().join("auth.json")).unwrap()).unwrap();
            let keys: Vec<&String> = auth.as_object().map(|o| o.keys().collect()).unwrap_or_default();
            assert_eq!(keys, vec!["anthropic"], "one synthetic default entry and no named account: {keys:?}");
            assert_eq!(auth["anthropic"]["access"], "synthetic-token");
            assert!(self.url.starts_with("http://127.0.0.1:"), "a loopback provider");
        }

        fn session_config(&self) -> SessionConfig {
            SessionConfig {
                cwd: Some(self.root.path().join("work")),
                env: None,
                model_override: Some("claude-sonnet-4-5".into()),
                ..Default::default()
            }
        }

        /// A session whose first turn stops on the reset: its armed notice,
        /// and its entry in the crash record carrying the stop.
        async fn stopped_session(&self, d: &Daemon) -> (SocketTransport, String) {
            let (mut t, snap) =
                attach(&d.paths, Attach::Create { config: self.session_config(), mode: AttachMode::Mirror }).await;
            let id = snap.meta.id.to_string();
            t.send_from_self(SessionCommand::Submit { text: TASK.into(), attachments: vec![] })
                .await
                .unwrap();
            let want = armed_notice(self.resume_at());
            let (mut idle, mut notice) = (false, false);
            while !(idle && notice) {
                let env = tokio::time::timeout(Duration::from_secs(30), t.next_event())
                    .await
                    .expect("the stopped turn's end and its notice")
                    .expect("alive");
                match env.event {
                    SessionEventWire::Idle => idle = true,
                    SessionEventWire::SystemNotice(text) if text == want => notice = true,
                    SessionEventWire::Refused { reason, .. } => panic!("refused: {reason}"),
                    _ => {}
                }
            }
            wait_until("the record carries the stop", Duration::from_secs(10), || {
                let text = self.record_text();
                text.contains(&id) && text.contains("\"capacity_stop\"")
            })
            .await;
            assert_eq!(self.hits(), 0, "the stopped turn sent nothing");
            assert_eq!(self.trap.hits(), 0, "the trap: {:?}", self.trap.seen());
            (t, id)
        }

        fn record_text(&self) -> String {
            std::fs::read_to_string(self.run_dir().join("daemon.live.json")).unwrap_or_default()
        }

        /// The session's journal id in the record (its id when empty).
        fn journal_of(&self, id: &str) -> String {
            let record: Value = serde_json::from_str(&self.record_text()).expect("a record");
            let entry = record["sessions"]
                .as_array()
                .and_then(|s| s.iter().find(|e| e["id"] == id))
                .expect("the session in the record")
                .clone();
            match entry["journal_id"].as_str() {
                Some(j) if !j.is_empty() => j.to_string(),
                _ => id.to_string(),
            }
        }

        async fn listed(&self, d: &Daemon, id: &str) -> Option<SessionMeta> {
            SocketTransport::sessions(&d.paths.sock)
                .await
                .expect("the session list")
                .into_iter()
                .find(|m| m.id.to_string() == id)
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        fn bodies(&self) -> Vec<Value> {
            self.bodies.lock().unwrap().clone()
        }

        async fn past_reset(&self, by: Duration) {
            let at = self.resume_at() + chrono::Duration::from_std(by).unwrap();
            let left = (at - Utc::now()).to_std().unwrap_or_default();
            tokio::time::sleep(left).await;
        }

        /// Exactly one provider request, at or after the reset, carrying the
        /// capacity resume's text for this stop once and no other resume;
        /// still one a while later.
        async fn expect_one_resume(&self, id: &str) {
            let deadline = Instant::now()
                + (self.resume_at() - Utc::now()).to_std().unwrap_or_default()
                + Duration::from_secs(40);
            while self.hits() < 1 {
                assert!(Instant::now() < deadline, "session {id} was never resumed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(Utc::now() >= self.resume_at(), "not before the reset");
            let arrived = self.arrivals.lock().unwrap()[0];
            assert!(arrived >= self.resume_at(), "the resume arrived at {arrived}, before the reset {}", self.resume_at());
            tokio::time::sleep(Duration::from_secs(4)).await;
            let bodies = self.bodies();
            assert_eq!(self.hits(), 1, "one resume: {bodies:#?}");
            let texts = user_texts(&bodies[0]);
            assert_eq!(texts.first().map(String::as_str), Some(TASK), "{texts:?}");
            let resumes: Vec<&String> = texts.iter().filter(|t| t.starts_with("resume:")).collect();
            assert_eq!(resumes.len(), 1, "{texts:?}");
            assert!(resumes[0].starts_with(RESUME_PREFIX), "{resumes:?}");
            let due = format!("one was due back at {}", self.resume_at().format("%H:%M UTC"));
            assert!(resumes[0].contains(&due), "{resumes:?}");
        }

        /// A clean stop, waited for (its log lines are flushed then); its
        /// group is then killed and the daemon reaped.
        async fn stop(&self, mut d: Daemon) {
            SocketTransport::shutdown(&d.paths.sock, false).await.expect("shutdown");
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                if d.proc.exited() {
                    break;
                }
                assert!(Instant::now() < deadline, "the daemon did not stop");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        fn log_text(&self) -> String {
            let mut names: Vec<PathBuf> = std::fs::read_dir(self.base())
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("synaps.log")))
                .collect();
            names.sort();
            names.iter().map(|p| std::fs::read_to_string(p).unwrap_or_default()).collect()
        }

        fn log_count(&self, needle: &str) -> usize {
            self.log_text().lines().filter(|l| l.contains(needle)).count()
        }

        async fn wait_log(&self, needle: &str, n: usize) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while self.log_count(needle) < n {
                assert!(Instant::now() < deadline, "never logged {n}x {needle:?}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        /// The times of the log lines holding `needle` (each line starts
        /// with its RFC 3339 UTC time).
        fn logged_at(&self, needle: &str) -> Vec<DateTime<Utc>> {
            self.log_text()
                .lines()
                .filter(|l| l.contains(needle))
                .map(|l| {
                    let ts = l.split_whitespace().next().unwrap_or_default();
                    DateTime::parse_from_rfc3339(ts)
                        .unwrap_or_else(|e| panic!("a log line without its time ({e}): {l}"))
                        .with_timezone(&Utc)
                })
                .collect()
        }

        fn expect_log(&self, needle: &str, n: usize) {
            assert_eq!(self.log_count(needle), n, "{needle:?} in the daemon log");
        }

        /// After the last daemon stopped: one consumption and one dispatch,
        /// the filter's controls, and nothing at the trap.
        fn end_checks(&self) {
            self.end_checks_with(1);
        }

        fn end_checks_with(&self, resumes: usize) {
            self.expect_log(CONSUMED, resumes);
            self.expect_log(DISPATCHED, resumes);
            // No early consumption or dispatch, by the daemon's own clock.
            for needle in [CONSUMED, DISPATCHED] {
                for at in self.logged_at(needle) {
                    assert!(at >= self.resume_at(), "{needle:?} logged at {at}, before the reset {}", self.resume_at());
                }
            }
            for at in self.arrivals.lock().unwrap().iter() {
                assert!(*at >= self.resume_at(), "a request arrived at {at}, before the reset {}", self.resume_at());
            }
            assert!(self.log_count(CONTROL_ERROR) >= 1, "the ERROR control line is in the log");
            self.expect_log(CONTROL_INFO, 0);
            assert_eq!(self.hits(), resumes, "provider requests: {:#?}", self.bodies());
            assert_eq!(self.trap.hits(), 0, "the trap: {:?}", self.trap.seen());
        }
    }

    // ── helpers ─────────────────────────────────────────────────────────

    async fn wait_ping(paths: &DaemonPaths) -> agent_engine::session::socket_transport::Pong {
        let t0 = Instant::now();
        loop {
            if let Ok(p) = SocketTransport::ping(&paths.sock).await {
                return p;
            }
            assert!(t0.elapsed() < Duration::from_secs(60), "the daemon never answered");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait_until(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + within;
        while !done() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn attach(paths: &DaemonPaths, a: Attach) -> (SocketTransport, AttachSnapshot) {
        let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
        SocketTransport::attach(conn, a).await.expect("attached")
    }

    /// Hold the session's journal lock as another process would.
    fn hold_lock(path: &Path) -> std::fs::File {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .expect("the lock file");
        // SAFETY: flock on a file this test owns.
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "lock {}", path.display());
        f
    }

    /// The texts of a request body's user messages, in order.
    fn user_texts(body: &Value) -> Vec<String> {
        let mut out = Vec::new();
        for m in body["messages"].as_array().into_iter().flatten() {
            if m["role"] != "user" {
                continue;
            }
            match &m["content"] {
                Value::String(s) => out.push(s.clone()),
                Value::Array(blocks) => {
                    out.extend(blocks.iter().filter_map(|b| b["text"].as_str().map(str::to_string)))
                }
                _ => {}
            }
        }
        out
    }

    type Arrivals = Arc<Mutex<Vec<DateTime<Utc>>>>;

    async fn provider_stub() -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<Value>>>, Arrivals) {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let arrivals: Arrivals = Arc::default();
        let (hits_c, seen_c, arrivals_c) = (Arc::clone(&hits), Arc::clone(&seen), Arc::clone(&arrivals));
        let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
            let (hits, seen, arrivals) = (Arc::clone(&hits_c), Arc::clone(&seen_c), Arc::clone(&arrivals_c));
            async move {
                arrivals.lock().unwrap().push(Utc::now());
                hits.fetch_add(1, Ordering::SeqCst);
                if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                    seen.lock().unwrap().push(v);
                }
                (axum::http::StatusCode::OK, [("content-type", "text/event-stream")], SSE_HI.to_string())
                    .into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), hits, seen, arrivals)
    }

    /// The egress trap: every proxy variable of the child points here. It
    /// counts each connection at ACCEPT, before reading any byte, refuses it
    /// and never forwards anything. Every connection but the self-test's
    /// (which takes a one-shot token set just before it connects) trips the
    /// run ([`trip`]); its first request line, when one comes, is kept for
    /// the report. The listener outlives the case: the teardown stops the
    /// accept loop and drains its backlog ([`drain_traps`]).
    struct Trap {
        url: String,
        addr: std::net::SocketAddr,
        hits: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<String>>>,
        self_test_token: Arc<AtomicBool>,
        self_tests: Arc<AtomicUsize>,
        accepting: Arc<Mutex<TrapLoop>>,
    }

    /// A trap's accept loop: its stop, its task (which hands the listener
    /// back when it stops) and, once stopped, the listener.
    struct TrapLoop {
        stop: Option<oneshot::Sender<()>>,
        task: Option<tokio::task::JoinHandle<tokio::net::TcpListener>>,
        listener: Option<tokio::net::TcpListener>,
        hits: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<String>>>,
    }

    /// Every trap started since the last teardown took them.
    static TRAPS: Mutex<Vec<Arc<Mutex<TrapLoop>>>> = Mutex::new(Vec::new());

    /// Stop a trap's accept loop and keep its listener, with every
    /// connection still in its backlog. A loop that ended any other way
    /// trips the run.
    async fn stop_loop(accepting: &Mutex<TrapLoop>) {
        let (stop, task) = {
            let mut l = accepting.lock().unwrap_or_else(|p| p.into_inner());
            (l.stop.take(), l.task.take())
        };
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
        if let Some(task) = task {
            match task.await {
                Ok(listener) => accepting.lock().unwrap_or_else(|p| p.into_inner()).listener = Some(listener),
                Err(e) => trip(format!("the trap's accept loop ended abnormally: {e}")),
            }
        }
    }

    /// The teardown's drain, after the case's processes are killed and
    /// reaped: each trap started since the last teardown stops accepting,
    /// and every connection still queued on its listener is accepted,
    /// counted and trips the run. Returns how many it found.
    async fn drain_traps() -> usize {
        let traps = std::mem::take(&mut *TRAPS.lock().unwrap_or_else(|p| p.into_inner()));
        let mut drained = 0;
        for accepting in traps {
            stop_loop(&accepting).await;
            let (listener, hits, seen) = {
                let mut l = accepting.lock().unwrap_or_else(|p| p.into_inner());
                (l.listener.take(), Arc::clone(&l.hits), Arc::clone(&l.seen))
            };
            let Some(listener) = listener else { continue };
            let listener = listener.into_std().expect("the trap's listener");
            listener.set_nonblocking(true).expect("a non-blocking drain");
            loop {
                match listener.accept() {
                    Ok((_sock, peer)) => {
                        drained += 1;
                        let n = hits.fetch_add(1, Ordering::SeqCst) + 1;
                        let what = format!("trap connection #{n} at teardown from {peer}");
                        seen.lock().unwrap_or_else(|p| p.into_inner()).push(what.clone());
                        trip(what);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        trip(format!("the trap's drain could not accept: {e}"));
                        break;
                    }
                }
            }
        }
        drained
    }

    const SELF_TEST_PATH: &str = "/__capres_trap_self_test";

    impl Trap {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let hits: Arc<AtomicUsize> = Arc::default();
            let seen: Arc<Mutex<Vec<String>>> = Arc::default();
            let self_test_token: Arc<AtomicBool> = Arc::default();
            let self_tests: Arc<AtomicUsize> = Arc::default();
            let (stop, mut stopped) = oneshot::channel::<()>();
            let (hits_c, seen_c) = (Arc::clone(&hits), Arc::clone(&seen));
            let (token, self_tests_c) = (Arc::clone(&self_test_token), Arc::clone(&self_tests));
            let task = tokio::spawn(async move {
                let (hits, seen, self_tests) = (hits_c, seen_c, self_tests_c);
                loop {
                    let got = tokio::select! {
                        biased;
                        _ = &mut stopped => None,
                        got = listener.accept() => Some(got),
                    };
                    let (mut sock, peer) = match got {
                        None => return listener,
                        Some(Ok(conn)) => conn,
                        Some(Err(e)) => {
                            trip(format!("the trap could not accept a connection: {e}"));
                            return listener;
                        }
                    };
                    // Counted here, before any byte is read.
                    let n = if token.swap(false, Ordering::SeqCst) {
                        self_tests.fetch_add(1, Ordering::SeqCst);
                        None
                    } else {
                        let n = hits.fetch_add(1, Ordering::SeqCst) + 1;
                        let what = format!("trap connection #{n} at accept from {peer}");
                        seen.lock().unwrap().push(what.clone());
                        trip(what);
                        Some(n)
                    };
                    let seen = Arc::clone(&seen);
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 2048];
                        let got = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf))
                            .await
                            .ok()
                            .and_then(Result::ok)
                            .unwrap_or(0);
                        if let Some(n) = n {
                            let first = String::from_utf8_lossy(&buf[..got]).lines().next().unwrap_or("").to_string();
                            seen.lock().unwrap().push(format!("trap connection #{n} first line: {first:?}"));
                        }
                        let _ = sock
                            .write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                            .await;
                    });
                }
            });
            let accepting = Arc::new(Mutex::new(TrapLoop {
                stop: Some(stop),
                task: Some(task),
                listener: None,
                hits: Arc::clone(&hits),
                seen: Arc::clone(&seen),
            }));
            TRAPS.lock().unwrap_or_else(|p| p.into_inner()).push(Arc::clone(&accepting));
            Self { url: format!("http://{addr}"), addr, hits, seen, self_test_token, self_tests, accepting }
        }

        /// Stop accepting, as a case's end would leave it: a connection
        /// made after this stays in the listener's backlog for the
        /// teardown's drain.
        async fn stop_accepting(&self) {
            stop_loop(&self.accepting).await;
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }

        /// A proxied request to a TEST-NET address reaches the trap (on its
        /// own token, counted apart) and is refused: the trap works. Nothing
        /// leaves the host.
        async fn self_test(&self) {
            let before = self.self_tests.load(Ordering::SeqCst);
            let client = reqwest::Client::builder()
                .proxy(reqwest::Proxy::all(&self.url).unwrap())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            self.self_test_token.store(true, Ordering::SeqCst);
            let status = client
                .get(format!("http://192.0.2.1{SELF_TEST_PATH}"))
                .send()
                .await
                .map(|r| r.status().as_u16());
            assert_eq!(status.ok(), Some(403), "the trap refuses");
            assert_eq!(self.self_tests.load(Ordering::SeqCst), before + 1, "the trap saw its self-test");
            assert!(!self.self_test_token.load(Ordering::SeqCst), "the self-test's token was taken");
        }
    }
}
