//! `synaps status` — show account usage and reset times.

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Refresh-if-needed via the shared auth machinery (honors profiles,
    // fs4 file locking, and persists the rotated token). Previously this
    // read auth.json raw from the base dir — expired tokens produced a
    // useless HTTP 401 even though the user was logged in.
    let client = reqwest::Client::new();
    let creds = synaps_cli::auth::ensure_fresh_token(&client)
        .await
        .map_err(|e| format!("Not logged in ({}). Run `synaps login` first.", e))?;
    let access = creds.access;

    let resp = client.get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {}", access))
        .header("anthropic-beta", "oauth-2025-04-20")
        .send().await?;

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
                if hours > 24 { format!("{}d {}h", hours / 24, hours % 24) }
                else if hours > 0 { format!("{}h {}m", hours, mins) }
                else { format!("{}m", diff.num_minutes()) }
            } else { "—".to_string() };

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
