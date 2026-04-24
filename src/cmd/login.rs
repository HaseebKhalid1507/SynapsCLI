use synaps_cli::auth;

pub async fn run(profile: Option<String>, provider: Option<String>) {
    if let Some(ref prof) = profile {
        synaps_cli::config::set_profile(Some(prof.clone()));
    }

    let _log_guard = synaps_cli::logging::init_logging();

    // If a specific provider was requested, skip the dashboard
    if let Some(ref p) = provider {
        match p.as_str() {
            "anthropic" | "claude" => {
                run_anthropic_login().await;
                return;
            }
            "openai" | "chatgpt" => {
                run_openai_login().await;
                return;
            }
            _ => {
                eprintln!("\x1b[31m✗ Unknown provider: {}\x1b[0m", p);
                eprintln!("  Available: anthropic, openai");
                std::process::exit(1);
            }
        }
    }

    // Show the provider dashboard
    show_dashboard().await;
}

// ── Dashboard ──────────────────────────────────────────────────────────────

/// Represents a selectable item in the dashboard.
struct DashboardItem {
    label: String,
    status: String,
    provider_id: &'static str, // "anthropic" or "openai"
}

fn oauth_status_line(creds: Option<&auth::OAuthCredentials>) -> String {
    match creds {
        Some(c) if c.auth_type == "oauth" && !c.access.is_empty() => {
            if auth::is_token_expired(c) {
                "\x1b[33m⚠ Token expired\x1b[0m".to_string()
            } else {
                format!("\x1b[32m✓ Logged in\x1b[0m — expires {}", format_expiry(c.expires))
            }
        }
        _ => "\x1b[31m✗ Not logged in\x1b[0m".to_string(),
    }
}

async fn show_dashboard() {
    use crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyEvent},
        execute,
        terminal::{self, ClearType},
    };
    use std::io::Write;

    // Load auth state
    let auth_file = auth::load_auth().ok().flatten();
    let anthropic_creds = auth_file.as_ref().map(|a| &a.anthropic);
    let openai_creds = auth_file.as_ref().and_then(|a| a.openai.as_ref());

    // Load API key provider status
    let provider_keys = synaps_cli::config::get_provider_keys();
    let providers = synaps_cli::runtime::openai::registry::list_providers(&provider_keys);
    let configured_api: Vec<_> = providers
        .iter()
        .filter(|(key, _, has_key, _)| *has_key && *key != "openai")
        .collect();

    // Build selectable items
    let items = vec![
        DashboardItem {
            label: "Anthropic (Claude)".to_string(),
            status: oauth_status_line(anthropic_creds),
            provider_id: "anthropic",
        },
        DashboardItem {
            label: "OpenAI (ChatGPT)".to_string(),
            status: oauth_status_line(openai_creds),
            provider_id: "openai",
        },
    ];

    let mut selected: usize = 0;
    let mut stderr = std::io::stderr();

    // Enter raw mode for arrow key input
    terminal::enable_raw_mode().ok();

    // Helper: write a line padded to fit inside the box (50-char inner width)
    // `visible_len` is the number of printable characters in `content`
    let box_line = |out: &mut std::io::Stderr, content: &str, visible_len: usize| {
        let pad = 50usize.saturating_sub(visible_len);
        write!(out, "  \x1b[1m║\x1b[0m{}{}\x1b[1m║\x1b[0m\r\n", content, " ".repeat(pad)).ok();
    };

    loop {
        // Clear and redraw
        execute!(stderr, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All)).ok();

        write!(stderr, "\r\n").ok();
        write!(stderr, "  \x1b[1m╔══════════════════════════════════════════════════╗\x1b[0m\r\n").ok();
        write!(stderr, "  \x1b[1m║              SynapsCLI — Login                   ║\x1b[0m\r\n").ok();
        write!(stderr, "  \x1b[1m╠══════════════════════════════════════════════════╣\x1b[0m\r\n").ok();
        box_line(&mut stderr, "", 0);
        box_line(&mut stderr, "  \x1b[1;4mOAuth Providers\x1b[0m", 17);
        box_line(&mut stderr, "", 0);

        for (i, item) in items.iter().enumerate() {
            let sel = i == selected;
            let cursor_char = if sel { "▸" } else { " " };
            if sel {
                let line = format!("   \x1b[1;36m{} {}\x1b[0m", cursor_char, item.label);
                // visible: 3 spaces + arrow + space + label
                let vlen = 3 + 2 + 1 + item.label.len();
                box_line(&mut stderr, &line, vlen);
            } else {
                let line = format!("   {} {}", cursor_char, item.label);
                let vlen = 3 + 2 + 1 + item.label.len();
                box_line(&mut stderr, &line, vlen);
            }
            // Status line (contains ANSI escapes — compute visible length manually)
            let status_text = &item.status;
            let status_visible = strip_ansi_len(status_text);
            let line = format!("      {}", status_text);
            box_line(&mut stderr, &line, 6 + status_visible);
            box_line(&mut stderr, "", 0);
        }

        // Only show API key providers if any are configured
        if !configured_api.is_empty() {
            box_line(&mut stderr, "  \x1b[1;4mAPI Key Providers\x1b[0m", 19);
            for (key, name, _, _) in &configured_api {
                let line = format!("   \x1b[32m✓\x1b[0m {} \x1b[2m({})\x1b[0m", name, key);
                // visible: 3 + "✓" + space + name + space + "(" + key + ")"
                let vlen = 3 + 2 + 1 + name.len() + 1 + 1 + key.len() + 1;
                box_line(&mut stderr, &line, vlen);
            }
            box_line(&mut stderr, "", 0);
        }

        write!(stderr, "  \x1b[1m╚══════════════════════════════════════════════════╝\x1b[0m\r\n").ok();
        write!(stderr, "\r\n").ok();
        write!(stderr, "  \x1b[2m↑↓ navigate  ⏎ select  esc cancel\x1b[0m\r\n").ok();
        stderr.flush().ok();

        // Read key event
        match event::read() {
            Ok(Event::Key(KeyEvent { code, .. })) => match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if selected > 0 {
                        selected -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if selected < items.len() - 1 {
                        selected += 1;
                    }
                }
                KeyCode::Enter => {
                    terminal::disable_raw_mode().ok();
                    // Clear the dashboard
                    execute!(stderr, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All)).ok();
                    let provider_id = items[selected].provider_id;
                    match provider_id {
                        "anthropic" => run_anthropic_login().await,
                        "openai" => run_openai_login().await,
                        _ => {}
                    }
                    return;
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    terminal::disable_raw_mode().ok();
                    execute!(stderr, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All)).ok();
                    eprintln!("  Cancelled.");
                    return;
                }
                _ => {}
            },
            _ => {}
        }
    }
}

/// Count visible characters in a string, ignoring ANSI escape sequences.
fn strip_ansi_len(s: &str) -> usize {
    let mut len = 0;
    let mut in_escape = false;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            in_escape = true;
            i += 1;
            continue;
        }
        if in_escape {
            if bytes[i] == b'm' || bytes[i] == b'\\' {
                in_escape = false;
            }
            i += 1;
            continue;
        }
        // Handle multi-byte UTF-8 (like ✓, ✗, ▸, ⚠)
        let ch = s[i..].chars().next().unwrap_or(' ');
        len += 1;
        i += ch.len_utf8();
    }
    len
}

// ── Provider Login Flows ──────────────────────────────────────────────────

async fn run_anthropic_login() {
    eprintln!("\n  \x1b[1mSign in with your Claude account\x1b[0m");
    eprintln!("  (Pro, Max, Team, or Enterprise)\n");

    if let Ok(Some(existing)) = auth::load_auth() {
        if !auth::is_token_expired(&existing.anthropic) {
            eprintln!("  \x1b[33m⚠ Already logged in with a valid token.\x1b[0m");
            eprintln!("    Expires: {}", format_expiry(existing.anthropic.expires));
            eprintln!("    Continuing will replace your current credentials.\n");
        } else {
            eprintln!("  \x1b[33m⚠ Existing token is expired. Logging in fresh.\x1b[0m\n");
        }
    }

    match auth::login().await {
        Ok(creds) => {
            eprintln!("\n  \x1b[32m✓ Anthropic login successful!\x1b[0m");
            eprintln!("    Token saved to: {}", auth::auth_file_path().display());
            eprintln!("    Expires: {}\n", format_expiry(creds.expires));
        }
        Err(e) => {
            eprintln!("\n  \x1b[31m✗ Login failed: {}\x1b[0m\n", e);
            std::process::exit(1);
        }
    }
}

async fn run_openai_login() {
    eprintln!("\n  \x1b[1mSign in with your ChatGPT account\x1b[0m");
    eprintln!("  (Plus, Pro, Team, or Enterprise)\n");

    if let Ok(Some(existing)) = auth::load_openai_auth() {
        if !auth::is_token_expired(&existing) {
            eprintln!("  \x1b[33m⚠ Already logged in with a valid OpenAI token.\x1b[0m");
            eprintln!("    Expires: {}", format_expiry(existing.expires));
            eprintln!("    Continuing will replace your current credentials.\n");
        } else {
            eprintln!("  \x1b[33m⚠ Existing OpenAI token is expired. Logging in fresh.\x1b[0m\n");
        }
    }

    match auth::login_openai().await {
        Ok(creds) => {
            eprintln!("\n  \x1b[32m✓ OpenAI login successful!\x1b[0m");
            eprintln!("    Token saved to: {}", auth::auth_file_path().display());
            eprintln!("    Expires: {}", format_expiry(creds.expires));
            eprintln!("    Use with: \x1b[1m/model openai/gpt-4.1\x1b[0m\n");
        }
        Err(e) => {
            eprintln!("\n  \x1b[31m✗ Login failed: {}\x1b[0m\n", e);
            std::process::exit(1);
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn format_expiry(expires_millis: u64) -> String {
    let secs = expires_millis / 1000;
    let now = synaps_cli::epoch_secs();

    if secs <= now {
        return "expired".to_string();
    }

    let remaining = secs - now;
    let days = remaining / 86400;
    let hours = (remaining % 86400) / 3600;

    if days > 0 {
        format!("in {} days, {} hours", days, hours)
    } else if hours > 0 {
        format!("in {} hours", hours)
    } else {
        let mins = remaining / 60;
        format!("in {} minutes", mins)
    }
}
