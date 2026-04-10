//! `synaps login` — OAuth login for SynapsCLI.
//!
//! Opens browser to claude.ai, captures the OAuth callback,
//! exchanges code for tokens, and saves to ~/.pi/agent/auth.json.

use synaps_cli::auth;

#[tokio::main]
async fn main() {
    eprintln!("╔══════════════════════════════════════╗");
    eprintln!("║        SynapsCLI — Login             ║");
    eprintln!("╠══════════════════════════════════════╣");
    eprintln!("║  Sign in with your Claude account    ║");
    eprintln!("║  (Pro, Max, Team, or Enterprise)     ║");
    eprintln!("╚══════════════════════════════════════╝");

    // Check if already logged in
    if let Ok(Some(existing)) = auth::load_auth() {
        if !auth::is_token_expired(&existing.anthropic) {
            eprintln!("\n\x1b[33m⚠ Already logged in with a valid token.\x1b[0m");
            eprintln!("  Expires: {}", format_expiry(existing.anthropic.expires));
            eprintln!("  Continuing will replace your current credentials.\n");
        } else {
            eprintln!("\n\x1b[33m⚠ Existing token is expired. Logging in fresh.\x1b[0m\n");
        }
    }

    match auth::login().await {
        Ok(creds) => {
            eprintln!("\n\x1b[32m✓ Login successful!\x1b[0m");
            eprintln!("  Token saved to: {}", auth::auth_file_path().display());
            eprintln!("  Expires: {}", format_expiry(creds.expires));
            eprintln!("\n  You can now use SynapsCLI. 🎉\n");
        }
        Err(e) => {
            eprintln!("\n\x1b[31m✗ Login failed: {}\x1b[0m", e);
            eprintln!("  Please try again.\n");
            std::process::exit(1);
        }
    }
}

fn format_expiry(expires_millis: u64) -> String {
    let secs = expires_millis / 1000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

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
