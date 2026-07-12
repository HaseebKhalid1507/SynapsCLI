use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::Command;
use synaps_cli::{auth, config};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthKind {
    OAuth,
    Cloud,
    ApiKey,
}

#[derive(Debug, Clone, Copy)]
struct LoginProvider {
    key: &'static str,
    name: &'static str,
    description: &'static str,
    auth_kind: AuthKind,
    recommended: bool,
}

const LOGIN_BANNER: &[&str] = &[
    " ███████ ██    ██ ███    ██  █████  ██████  ███████",
    " ██       ██  ██  ████   ██ ██   ██ ██   ██ ██    ",
    " ███████   ████   ██ ██  ██ ███████ ██████  ███████",
    "      ██    ██    ██  ██ ██ ██   ██ ██           ██",
    " ███████    ██    ██   ████ ██   ██ ██      ███████",
];
const LOGIN_PICKER_PADDING: &str = "  ";

pub async fn run(profile: Option<String>, provider_key: Option<String>) {
    if let Some(ref prof) = profile {
        synaps_cli::config::set_profile(Some(prof.clone()));
    }

    let _log_guard = synaps_cli::logging::init_logging();

    let providers = login_providers();
    let selected = if let Some(ref key) = provider_key {
        match find_provider(&providers, key) {
            Some(p) => p,
            None => {
                eprintln!("error: unknown provider '{}'", key);
                eprintln!("valid provider keys:");
                for p in &providers {
                    eprintln!("  {}", p.key);
                }
                std::process::exit(1);
            }
        }
    } else {
        match select_provider(&providers) {
            Ok(provider) => provider,
            Err(e) => {
                eprintln!("\n\x1b[31m✗ Login failed: {}\x1b[0m", e);
                std::process::exit(1);
            }
        }
    };

    match selected.auth_kind {
        AuthKind::OAuth => run_oauth_login(selected, profile).await,
        AuthKind::Cloud => run_cloud_login(selected, profile).await,
        AuthKind::ApiKey => run_api_key_login(selected, profile),
    }
}

async fn run_oauth_login(provider: LoginProvider, profile: Option<String>) {
    eprintln!("╔══════════════════════════════════════╗");
    eprintln!("║        SynapsCLI — Login             ║");
    eprintln!("╠══════════════════════════════════════╣");
    eprintln!("║  Sign in with {:<20}║", provider.name);
    eprintln!("║  {:<36}║", provider.description);
    eprintln!("╚══════════════════════════════════════╝");

    if let Ok(Some(existing)) = auth::load_provider_auth(oauth_storage_key(provider)) {
        if !auth::is_token_expired(&existing) {
            eprintln!("\n\x1b[33m⚠ Already logged in with a valid token.\x1b[0m");
            eprintln!("  Expires: {}", format_expiry(existing.expires));
            eprintln!("  Continuing will replace your current credentials.\n");
        } else {
            eprintln!("\n\x1b[33m⚠ Existing token is expired. Logging in fresh.\x1b[0m\n");
        }
    }

    let result = match auth::provider::parse_cli_provider(provider.key) {
        Ok(id) => auth::provider::login(id).await,
        Err(_) => Err(format!("No OAuth login handler for {}", provider.name)),
    };

    match result {
        Ok(creds) => {
            eprintln!("\n\x1b[32m✓ Login successful!\x1b[0m");
            eprintln!("  Token saved to: {}", auth::auth_file_path().display());
            eprintln!("  Expires: {}", format_expiry(creds.expires));
            eprintln!("\n  You can now use SynapsCLI.\n");
            continue_to_main_app(profile);
        }
        Err(e) => {
            eprintln!("\n\x1b[31m✗ Login failed: {}\x1b[0m", e);
            eprintln!("  Please try again.\n");
            std::process::exit(1);
        }
    }
}

fn oauth_storage_key(provider: LoginProvider) -> &'static str {
    auth::provider::parse_cli_provider(provider.key)
        .map(|id| id.as_str())
        .unwrap_or(provider.key)
}

async fn run_cloud_login(provider: LoginProvider, profile: Option<String>) {
    let result = match provider.key {
        "aws-bedrock" => login_aws_bedrock().await,
        "azure-openai" => Err("registration_required: configure SYNAPS_AZURE_CLIENT_ID and Azure tenant/subscription/resource context; Synaps does not ship another product's client ID".into()),
        "google-vertex" => Err("registration_required: configure SYNAPS_GOOGLE_VERTEX_CLIENT_ID and project/location; Synaps does not ship another product's client ID".into()),
        _ => Err("cloud provider setup is unavailable".into()),
    };
    match result {
        Ok(()) => {
            eprintln!("\n\x1b[32m✓ {} login successful\x1b[0m", provider.name);
            eprintln!("  Credentials saved to the broker credential store.");
            continue_to_main_app(profile);
        }
        Err(message) => {
            eprintln!(
                "\n\x1b[31m✗ {} login failed: {}\x1b[0m",
                provider.name, message
            );
        }
    }
}

fn required_env(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("missing {name}"))
}

async fn login_aws_bedrock() -> Result<(), String> {
    use auth::aws_bedrock::{AwsApi, AwsHttpApi, TokenGrant};
    let start = required_env("SYNAPS_AWS_SSO_START_URL")?;
    let sso_region = required_env("SYNAPS_AWS_SSO_REGION")?;
    let bedrock_region = required_env("SYNAPS_AWS_BEDROCK_REGION")?;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "cannot initialize HTTPS client")?;
    let api = AwsHttpApi::new(http, &sso_region);
    let client = api
        .register_client(&sso_region)
        .await
        .map_err(|e| e.to_string())?;
    let device = api
        .start_device_authorization(&client, &start)
        .await
        .map_err(|e| e.to_string())?;
    eprintln!(
        "Open {} and enter code {}",
        device.verification_uri, device.user_code
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(device.expires_in);
    let mut interval = device.interval.max(1);
    let token = loop {
        if std::time::Instant::now() >= deadline {
            return Err("device authorization expired".into());
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        match api
            .create_token(
                &client,
                &sso_region,
                TokenGrant::DeviceCode(device.device_code()),
            )
            .await
        {
            Ok(token) => break token,
            Err(auth::aws_bedrock::AwsError::AuthorizationPending) => {}
            Err(auth::aws_bedrock::AwsError::SlowDown) => interval = interval.saturating_add(5),
            Err(e) => return Err(e.to_string()),
        }
    };
    let accounts = api
        .list_accounts(&sso_region, token.access())
        .await
        .map_err(|e| e.to_string())?;
    let account = choose(
        "account",
        accounts
            .iter()
            .map(|a| (a.id.as_str(), a.name.as_str()))
            .collect(),
        std::env::var("SYNAPS_AWS_ACCOUNT_ID").ok().as_deref(),
    )?;
    let roles = api
        .list_account_roles(&sso_region, token.access(), &account)
        .await
        .map_err(|e| e.to_string())?;
    let role = choose(
        "role",
        roles.iter().map(|r| (r.name.as_str(), "")).collect(),
        std::env::var("SYNAPS_AWS_ROLE_NAME").ok().as_deref(),
    )?;
    let credentials = api
        .get_role_credentials(&sso_region, token.access(), &account, &role)
        .await
        .map_err(|e| e.to_string())?;
    let config = auth::AwsBedrockConfig::new(start, sso_region, account, role, bedrock_region)?;
    let state = serde_json::json!({
        "config": config, "access_key": credentials.access_key(), "secret_key": credentials.secret_key(),
        "session_token": credentials.session_token(), "expires_at": credentials.expires_at,
        "registered_client": {"id": client.id(), "secret": client.secret(), "expires_at": client.expires_at},
        "sso_access_token": token.access(), "sso_refresh_token": token.refresh(),
        "sso_expires_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64 + token.expires_in * 1000
    });
    auth::save_cloud_state("aws-bedrock", &state)
}

fn choose(
    kind: &str,
    values: Vec<(&str, &str)>,
    configured: Option<&str>,
) -> Result<String, String> {
    if let Some(want) = configured {
        return values
            .iter()
            .find(|(id, _)| *id == want)
            .map(|(id, _)| (*id).to_string())
            .ok_or_else(|| format!("configured {kind} is not assigned"));
    }
    if values.len() == 1 {
        return Ok(values[0].0.to_string());
    }
    if !io::stdin().is_terminal() {
        return Err(format!(
            "multiple {kind}s; set SYNAPS_AWS_{}",
            if kind == "account" {
                "ACCOUNT_ID"
            } else {
                "ROLE_NAME"
            }
        ));
    }
    eprintln!("Select {kind}:");
    for (i, (id, label)) in values.iter().enumerate() {
        eprintln!("  {}. {} {}", i + 1, id, label);
    }
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let i: usize = line.trim().parse().map_err(|_| "invalid selection")?;
    values
        .get(i.saturating_sub(1))
        .map(|v| v.0.to_string())
        .ok_or_else(|| "invalid selection".into())
}

fn run_api_key_login(provider: LoginProvider, profile: Option<String>) {
    eprintln!("╔══════════════════════════════════════╗");
    eprintln!("║        SynapsCLI — Login             ║");
    eprintln!("╠══════════════════════════════════════╣");
    eprintln!("║  Add API key for {:<18}║", provider.name);
    eprintln!("║  OpenAI-compatible endpoint          ║");
    eprintln!("╚══════════════════════════════════════╝");

    let config_key = format!("provider.{}", provider.key);
    if let Some(existing) = config::load_config().provider_keys.get(provider.key) {
        if !existing.is_empty() {
            eprintln!(
                "\n\x1b[33m⚠ API key already configured for {}.\x1b[0m",
                provider.name
            );
            eprintln!("  Continuing will replace provider.{}.\n", provider.key);
        }
    }

    eprintln!("\nPaste your {} API key.", provider.name);
    eprint!("{}: ", config_key);
    let _ = io::stderr().flush();

    let api_key = match read_secret_line() {
        Ok(api_key) => api_key,
        Err(e) => {
            eprintln!("\n\x1b[31m✗ Login failed: {}\x1b[0m", e);
            std::process::exit(1);
        }
    };

    let api_key = api_key.trim();
    if api_key.is_empty() {
        eprintln!("\n\x1b[31m✗ Login failed: API key cannot be empty\x1b[0m");
        std::process::exit(1);
    }

    save_api_key(&config_key, provider, api_key);
    continue_to_main_app(profile);
}

fn read_secret_line() -> Result<String, String> {
    if !io::stdin().is_terminal() {
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|e| e.to_string())?;
        return Ok(input);
    }

    let _raw_guard = RawModeGuard::new()?;
    let mut input = String::new();

    loop {
        match event::read().map_err(|e| e.to_string())? {
            Event::Key(key) => match key.code {
                KeyCode::Enter => {
                    eprint!("\r\n");
                    let _ = io::stderr().flush();
                    return Ok(input);
                }
                KeyCode::Esc => {
                    eprint!("\r\n");
                    return Err("Login canceled".to_string());
                }
                KeyCode::Backspace => {
                    if input.pop().is_some() {
                        eprint!("\x08 \x08");
                        let _ = io::stderr().flush();
                    }
                }
                KeyCode::Char(c) if !c.is_control() => {
                    input.push(c);
                    eprint!("*");
                    let _ = io::stderr().flush();
                }
                _ => {}
            },
            Event::Paste(text) => {
                input.push_str(&text);
                eprint!("{}", "*".repeat(text.chars().count()));
                let _ = io::stderr().flush();
            }
            _ => {}
        }
    }
}

fn main_app_args(profile: Option<String>) -> Vec<String> {
    match profile {
        Some(profile) if !profile.trim().is_empty() => vec!["--profile".to_string(), profile],
        _ => Vec::new(),
    }
}

fn main_app_launch_targets(current_exe: Option<PathBuf>) -> Vec<PathBuf> {
    let mut targets = vec![PathBuf::from("synaps")];
    if let Some(current_exe) = current_exe {
        if current_exe != PathBuf::from("synaps") {
            targets.push(current_exe);
        }
    }
    targets
}

fn should_prompt_to_open_main_app(interactive: bool) -> bool {
    interactive
}

fn wait_for_enter_to_open_main_app() {
    eprintln!("Press Enter to open SynapsCLI…");
    let _ = io::stderr().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
}

fn continue_to_main_app(profile: Option<String>) {
    if !should_prompt_to_open_main_app(io::stdin().is_terminal() && io::stderr().is_terminal()) {
        return;
    }

    wait_for_enter_to_open_main_app();
    launch_main_app_or_exit(profile);
}

fn launch_main_app_or_exit(profile: Option<String>) -> ! {
    let args = main_app_args(profile);
    let targets = main_app_launch_targets(std::env::current_exe().ok());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        for target in &targets {
            let err = Command::new(target).args(&args).exec();
            eprintln!("Failed to launch {}: {}", target.display(), err);
        }
        std::process::exit(1);
    }

    #[cfg(not(unix))]
    {
        for target in &targets {
            match Command::new(target).args(&args).status() {
                Ok(status) => std::process::exit(status.code().unwrap_or(1)),
                Err(err) => eprintln!("Failed to launch {}: {}", target.display(), err),
            }
        }
        std::process::exit(1);
    }
}

fn save_api_key(_config_key: &str, provider: LoginProvider, api_key: &str) {
    // Static keys are broker-owned: persist into the broker's credential
    // store (auth.json, 0600, atomic merge write) rather than the plaintext
    // config file. Legacy `provider.<key>` config entries keep working via
    // broker-side discovery/migration.
    match auth::save_static_key(provider.key, api_key) {
        Ok(()) => {
            eprintln!("\n\x1b[32m✓ API key saved!\x1b[0m");
            eprintln!("  Provider: {}", provider.key);
            eprintln!(
                "  Broker credential store: {}",
                auth::auth_file_path().display()
            );
            eprintln!("\n  Use models as `{}/<model-id>`.\n", provider.key);
        }
        Err(e) => {
            eprintln!("\n\x1b[31m✗ Login failed: {}\x1b[0m", e);
            std::process::exit(1);
        }
    }
}

fn find_provider(providers: &[LoginProvider], key: &str) -> Option<LoginProvider> {
    let key_lower = key.to_lowercase();
    providers
        .iter()
        .find(|p| p.key.to_lowercase() == key_lower)
        .copied()
}

fn login_providers() -> Vec<LoginProvider> {
    let mut oauth: Vec<_> = auth::provider::registry().iter().copied().collect();
    oauth.sort_by_key(|provider| !provider.recommended);
    let mut providers: Vec<_> = oauth
        .into_iter()
        .map(|provider| LoginProvider {
            key: if provider.id == auth::OAuthProviderId::Anthropic {
                "claude"
            } else {
                provider.id.as_str()
            },
            name: provider.display_name,
            description: provider.description,
            auth_kind: AuthKind::OAuth,
            recommended: provider.recommended,
        })
        .collect();

    providers.extend(
        synaps_cli::auth::cloud::cloud_provider_descriptors()
            .iter()
            .map(|provider| LoginProvider {
                key: provider.id.as_str(),
                name: provider.display_name,
                description: if provider.registration_required {
                    "typed broker OAuth (registration required)"
                } else {
                    "IAM Identity Center + broker-side SigV4"
                },
                auth_kind: AuthKind::Cloud,
                recommended: false,
            }),
    );

    providers.extend(
        synaps_cli::runtime::openai::registry::providers()
            .iter()
            .map(|provider| LoginProvider {
                key: provider.key,
                name: provider.name,
                description: "API key",
                auth_kind: AuthKind::ApiKey,
                recommended: false,
            }),
    );

    providers
}

fn select_provider(providers: &[LoginProvider]) -> Result<LoginProvider, String> {
    if providers.is_empty() {
        return Err("No login providers are available".to_string());
    }

    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        let visible = filtered_provider_indices(providers, "");
        render_provider_picker(providers, &visible, 0, "");
        return Ok(providers[0]);
    }

    let _raw_guard = RawModeGuard::new()?;
    let mut selected = 0usize;
    let mut query = String::new();

    loop {
        let visible = filtered_provider_indices(providers, &query);
        if selected >= visible.len() {
            selected = visible.len().saturating_sub(1);
        }

        render_provider_picker(providers, &visible, selected, &query);
        match event::read().map_err(|e| e.to_string())? {
            Event::Key(key) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(visible.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    eprint!("\r\n");
                    if let Some(provider_idx) = visible.get(selected) {
                        return Ok(providers[*provider_idx]);
                    }
                }
                KeyCode::Esc => {
                    eprint!("\r\n");
                    return Err("Login canceled".to_string());
                }
                KeyCode::Backspace => {
                    query.pop();
                    selected = 0;
                }
                KeyCode::Char(c) if !c.is_control() => {
                    query.push(c);
                    selected = 0;
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn filtered_provider_indices(providers: &[LoginProvider], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return (0..providers.len()).collect();
    }

    providers
        .iter()
        .enumerate()
        .filter_map(|(idx, provider)| {
            let haystack = format!(
                "{} {} {}",
                provider.key, provider.name, provider.description
            )
            .to_lowercase();
            haystack.contains(&query).then_some(idx)
        })
        .collect()
}

fn render_provider_picker(
    providers: &[LoginProvider],
    visible: &[usize],
    selected: usize,
    query: &str,
) {
    eprint!("\x1b[2J\x1b[H");
    eprint!("\r\n");
    for line in LOGIN_BANNER {
        eprint!("{}{}\r\n", LOGIN_PICKER_PADDING, line);
    }
    eprint!("\r\n");
    eprint!("{}┌ Add credential\r\n", LOGIN_PICKER_PADDING);
    eprint!("{}│\r\n", LOGIN_PICKER_PADDING);
    eprint!("{}◇ Select provider\r\n", LOGIN_PICKER_PADDING);
    eprint!("{}│\r\n", LOGIN_PICKER_PADDING);
    eprint!("{}│ Search: {}\r\n", LOGIN_PICKER_PADDING, query);
    if visible.is_empty() {
        eprint!(
            "{}│ \x1b[2mNo providers match your search.\x1b[0m\r\n",
            LOGIN_PICKER_PADDING
        );
    }
    for (row, provider_idx) in visible.iter().enumerate() {
        let provider = providers[*provider_idx];
        let marker = if row == selected { "●" } else { "○" };
        let suffix = if provider.recommended {
            " (recommended)"
        } else {
            ""
        };
        let auth = match provider.auth_kind {
            AuthKind::OAuth => "oauth",
            AuthKind::Cloud => "cloud broker",
            AuthKind::ApiKey => "api key",
        };
        eprint!(
            "{}│ {} {}{} \x1b[2m{} · {}\x1b[0m\r\n",
            LOGIN_PICKER_PADDING, marker, provider.name, suffix, auth, provider.description
        );
    }
    eprint!("{}│\r\n", LOGIN_PICKER_PADDING);
    eprint!(
        "{}└ ↑/↓ to select • Enter: confirm • Type: search • Esc: cancel\r\n",
        LOGIN_PICKER_PADDING
    );
    let _ = io::stderr().flush();
}

struct RawModeGuard;

impl RawModeGuard {
    fn new() -> Result<Self, String> {
        enable_raw_mode().map_err(|e| e.to_string())?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_providers_include_all_typed_cloud_descriptors() {
        let providers = login_providers();
        for key in ["azure-openai", "aws-bedrock", "google-vertex"] {
            let provider = find_provider(&providers, key).expect("cloud descriptor missing");
            assert_eq!(provider.auth_kind, AuthKind::Cloud);
        }
    }

    #[test]
    fn relaunch_args_preserve_profile() {
        assert_eq!(
            main_app_args(Some("work".to_string())),
            vec!["--profile", "work"]
        );
    }

    #[test]
    fn relaunch_args_are_empty_without_profile() {
        assert!(main_app_args(None).is_empty());
    }

    #[test]
    fn relaunch_targets_prefer_path_synaps_then_current_exe() {
        let current = std::path::PathBuf::from("/tmp/synaps-current");
        let targets = main_app_launch_targets(Some(current.clone()));
        assert_eq!(targets, vec![std::path::PathBuf::from("synaps"), current]);
    }

    #[test]
    fn relaunch_targets_fallback_to_path_synaps_without_current_exe() {
        assert_eq!(
            main_app_launch_targets(None),
            vec![std::path::PathBuf::from("synaps")]
        );
    }

    #[test]
    fn should_prompt_to_open_main_app_only_for_interactive_success() {
        assert!(should_prompt_to_open_main_app(true));
        assert!(!should_prompt_to_open_main_app(false));
    }

    #[test]
    fn login_providers_include_claude_oauth_first() {
        let providers = login_providers();

        assert_eq!(providers[0].key, "claude");
        assert_eq!(providers[0].auth_kind, AuthKind::OAuth);
        assert!(providers[0].recommended);
        // Non-recommended OAuth providers follow; HashMap registry order is not stable.
        assert!(
            providers
                .iter()
                .any(|p| p.key == "openai-codex" && p.auth_kind == AuthKind::OAuth),
            "openai-codex must remain in the OAuth login list"
        );
        assert!(
            providers
                .iter()
                .any(|p| p.key == "xai-auth" && p.auth_kind == AuthKind::OAuth),
            "xai-auth must remain in the OAuth login list"
        );
    }

    #[test]
    fn login_providers_include_openai_compatible_api_key_entries() {
        let providers = login_providers();

        assert!(
            providers
                .iter()
                .any(|provider| provider.key == "openrouter"
                    && provider.auth_kind == AuthKind::ApiKey)
        );
        assert!(providers
            .iter()
            .any(|provider| provider.key == "google" && provider.auth_kind == AuthKind::ApiKey));
    }

    #[test]
    fn non_interactive_provider_lookup_finds_openai_codex() {
        let providers = login_providers();
        let found = find_provider(&providers, "openai-codex");
        assert!(found.is_some());
        let p = found.unwrap();
        assert_eq!(p.key, "openai-codex");
        assert_eq!(p.auth_kind, AuthKind::OAuth);

        // case-insensitive
        assert!(find_provider(&providers, "OpenAI-Codex").is_some());

        // unknown key returns None
        assert!(find_provider(&providers, "totally-bogus-xyz").is_none());
    }

    #[test]
    fn login_providers_include_github_copilot_from_descriptor() {
        let providers = login_providers();
        let found = find_provider(&providers, "github-copilot")
            .expect("descriptor-driven login must surface github-copilot");
        assert_eq!(found.key, "github-copilot");
        assert_eq!(found.auth_kind, AuthKind::OAuth);
        assert_eq!(found.name, "GitHub Copilot");
        assert!(!found.recommended);
        // Storage key is canonical, not the claude-style alias rewrite.
        assert_eq!(oauth_storage_key(found), "github-copilot");
        // CLI aliases resolve through parse_cli_provider; picker key stays canonical.
        assert!(find_provider(&providers, "copilot").is_none());
        assert_eq!(
            auth::provider::parse_cli_provider("copilot")
                .unwrap()
                .as_str(),
            "github-copilot"
        );
    }

    #[test]
    fn login_providers_include_google_gemini_from_descriptor() {
        let providers = login_providers();
        let found = find_provider(&providers, "google-gemini")
            .expect("descriptor-driven login must surface google-gemini");
        assert_eq!(found.key, "google-gemini");
        assert_eq!(found.auth_kind, AuthKind::OAuth);
        assert_eq!(found.name, "Google Gemini (Code Assist)");
        assert!(!found.recommended);
        // Storage key is canonical.
        assert_eq!(oauth_storage_key(found), "google-gemini");
        // The static Google AI Studio API-key entry stays reachable at "google".
        let ai_studio = find_provider(&providers, "google")
            .expect("Google AI Studio API-key entry must remain reachable");
        assert_eq!(ai_studio.auth_kind, AuthKind::ApiKey);
        // CLI aliases route to google-gemini except the reserved "google".
        assert_eq!(
            auth::provider::parse_cli_provider("gemini")
                .unwrap()
                .as_str(),
            "google-gemini"
        );
        assert!(auth::provider::parse_cli_provider("google").is_err());
    }
}
