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
