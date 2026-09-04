//! `synaps status` — show account usage and reset times.

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Resolve the access token honoring the credential source: Local refreshes
    // auth.json (fs4-locked, persisted) exactly as before; Remote fetches from
    // the broker WITHOUT rotating the refresh token client-side — a stray
    // `synaps status` on a Remote client must never rotate the broker's token
    // out from under the fleet. (#158 A4)
    let client = reqwest::Client::new();
    let config = synaps_cli::config::load_config();
    let source = config.auth.credential_source();
    let cache = synaps_cli::auth::TokenCache::new();
    let access = synaps_cli::auth::resolve_access_token("anthropic", &source, &cache, &client)
        .await
        .map_err(|e| format!(
            "Could not get a token ({}). Run `synaps login`, or check auth.remote_endpoint / broker reachability.", e
        ))?;

    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {}", access))
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        if status.as_u16() == 401 {
            eprintln!("Token rejected (401) — run `synaps login` to re-authenticate.");
        } else {
            eprintln!("Failed to fetch usage: HTTP {}", status);
        }
        std::process::exit(1);
    }

    let data: serde_json::Value = resp.json().await?;

    fn print_usage(label: &str, data: &serde_json::Value) {
        if let Some(util) = data["utilization"].as_f64() {
            let resets = data["resets_at"].as_str().unwrap_or("—");
            let reset_display = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(resets) {
                let diff = dt.signed_duration_since(chrono::Utc::now());
                let hours = diff.num_hours();
                let mins = diff.num_minutes() % 60;
                if hours > 24 {
                    format!("{}d {}h", hours / 24, hours % 24)
                } else if hours > 0 {
                    format!("{}h {}m", hours, mins)
                } else {
                    format!("{}m", diff.num_minutes())
                }
            } else {
                "—".to_string()
            };

            let bar_width: usize = 30;
            let filled = ((util / 100.0) * bar_width as f64) as usize;
            let empty = bar_width.saturating_sub(filled);
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty));

            println!("  {}", label);
            println!("  {} {:.0}%", bar, util);
            println!("  resets in {}", reset_display);
            println!();
        }
    }

    println!();
    println!("  ⚡ Account Usage");
    println!();
    print_usage("5-hour window", &data["five_hour"]);
    print_usage("7-day window", &data["seven_day"]);
    print_usage("Sonnet (7-day)", &data["seven_day_sonnet"]);

    Ok(())
}

// ═══ `synaps status --memory` (§3.7) ═══════════════════════════════════════

use synaps_cli::core::memstat::{self, MemTotals, ProcMem, ProcRole};

#[derive(serde::Serialize)]
struct SessionMem {
    session_id: Option<String>,
    name: Option<String>,
    root_pid: u32,
    procs: Vec<ProcMem>,
    totals: MemTotals,
}

#[derive(serde::Serialize)]
struct MemoryReport {
    sessions: Vec<SessionMem>,
    totals: MemTotals,
}

fn collect(pid: Option<u32>) -> Result<MemoryReport, Box<dyn std::error::Error>> {
    let mut sessions = Vec::new();
    match pid {
        Some(p) => {
            let procs = memstat::tree(p)?;
            let totals = MemTotals::of(&procs);
            sessions.push(SessionMem {
                session_id: None,
                name: None,
                root_pid: p,
                procs,
                totals,
            });
        }
        None => {
            let mut regs = synaps_cli::events::registry::list_active_sessions();
            regs.sort_by(|a, b| a.started_at.cmp(&b.started_at));
            for reg in regs {
                let Ok(procs) = memstat::tree(reg.pid) else {
                    continue;
                };
                let totals = MemTotals::of(&procs);
                sessions.push(SessionMem {
                    session_id: Some(reg.session_id),
                    name: reg.name,
                    root_pid: reg.pid,
                    procs,
                    totals,
                });
            }
        }
    }
    let all: Vec<ProcMem> = sessions
        .iter()
        .flat_map(|s| s.procs.iter().cloned())
        .collect();
    let totals = MemTotals::of(&all);
    Ok(MemoryReport { sessions, totals })
}

fn mb(kb: u64) -> f64 {
    kb as f64 / 1024.0
}

fn role_label(role: &ProcRole) -> String {
    match role {
        ProcRole::Engine => "engine".into(),
        ProcRole::ExtensionSidecar { name } => format!("ext:{name}"),
        ProcRole::McpServer { name } => format!("mcp:{name}"),
        ProcRole::Shell => "shell".into(),
        ProcRole::Other => "other".into(),
    }
}

fn print_table(report: &MemoryReport) {
    if report.sessions.is_empty() {
        println!("No live sessions (registry empty) — pass --pid N to inspect a process tree.");
        return;
    }
    println!(
        "{:<8} {:<22} {:>8} {:>8} {:>8} {:>8} {:>4}  {}",
        "PID", "ROLE", "RSS MB", "PSS MB", "USS MB", "ANON MB", "THR", "CMD"
    );
    for s in &report.sessions {
        let label = match (&s.name, &s.session_id) {
            (Some(n), Some(id)) => format!("{n} ({id})"),
            (None, Some(id)) => id.clone(),
            _ => format!("pid {}", s.root_pid),
        };
        println!("── session {label}");
        for p in &s.procs {
            let cmd: String = p.cmd.chars().take(60).collect();
            println!(
                "{:<8} {:<22} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>4}  {}",
                p.pid,
                role_label(&p.role),
                mb(p.rss_kb),
                mb(p.pss_kb),
                mb(p.uss_kb),
                mb(p.anon_kb),
                p.threads,
                cmd
            );
        }
        let t = &s.totals;
        println!(
            "{:<8} {:<22} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>4}",
            "",
            format!("subtotal ({} procs)", t.procs),
            mb(t.rss_kb),
            mb(t.pss_kb),
            mb(t.uss_kb),
            mb(t.anon_kb),
            t.threads
        );
    }
    let t = &report.totals;
    println!(
        "{:<8} {:<22} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>4}",
        "",
        format!(
            "TOTAL ({} sessions, {} procs)",
            report.sessions.len(),
            t.procs
        ),
        mb(t.rss_kb),
        mb(t.pss_kb),
        mb(t.uss_kb),
        mb(t.anon_kb),
        t.threads
    );
}

/// `synaps status --memory [--json] [--pid N]`.
pub fn run_memory(json: bool, pid: Option<u32>) -> Result<(), Box<dyn std::error::Error>> {
    let report = collect(pid)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_table(&report);
    }
    Ok(())
}
