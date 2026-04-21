//! `synaps status` — show account usage and reset times.

use synaps_cli::config;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let auth_path = config::base_dir().join("auth.json");
    let content = std::fs::read_to_string(&auth_path)
        .map_err(|_| "Not logged in — run `synaps login` first")?;
    let auth: serde_json::Value = serde_json::from_str(&content)?;
    let access = auth["anthropic"]["access"].as_str()
        .ok_or("No OAuth token found — run `synaps login`")?;

    let client = reqwest::Client::new();
    let resp = client.get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {}", access))
        .header("anthropic-beta", "oauth-2025-04-20")
        .send().await?;

    if !resp.status().is_success() {
        eprintln!("Failed to fetch usage: HTTP {}", resp.status());
        std::process::exit(1);
    }

    let data: serde_json::Value = resp.json().await?;

    println!("╭─────────────────────────────────────╮");
    println!("│       SynapsCLI Usage Status        │");
    println!("├─────────────────────────────────────┤");

    fn print_row(label: &str, data: &serde_json::Value) {
        if let Some(util) = data["utilization"].as_f64() {
            let resets = data["resets_at"].as_str().unwrap_or("—");
            let reset_display = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(resets) {
                let now = chrono::Utc::now();
                let diff = dt.signed_duration_since(now);
                let hours = diff.num_hours();
                let mins = diff.num_minutes() % 60;
                if hours > 24 {
                    format!("{}d {}h", hours / 24, hours % 24)
                } else if hours > 0 {
                    format!("{}h {}m", hours, mins)
                } else {
                    format!("{}m", mins)
                }
            } else {
                "—".to_string()
            };

            let bar_width = 20usize;
            let filled = ((util / 100.0) * bar_width as f64) as usize;
            let empty = bar_width.saturating_sub(filled);
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty));

            println!("│ {:>10}  {} {:>3.0}%  ↻ {:>7} │", label, bar, util, reset_display);
        }
    }

    print_row("5-hour", &data["five_hour"]);
    print_row("7-day", &data["seven_day"]);
    print_row("Sonnet 7d", &data["seven_day_sonnet"]);

    println!("╰─────────────────────────────────────╯");

    Ok(())
}
