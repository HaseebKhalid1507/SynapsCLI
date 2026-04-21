use ratatui::text::Line;
use serde_json::Value;
use chrono::Local;
use synaps_cli::Session;


#[derive(Clone)]
pub(crate) enum ChatMessage {
    User(String),
    Thinking(String),
    Text(String),
    ToolUseStart(String, String),  // (tool_name, partial_input)
    ToolUse { tool_name: String, input: String },
    ToolResult { content: String, elapsed_ms: Option<u64> },
    Error(String),
    System(String),
    Event { source: String, severity: String, text: String },
}

pub(crate) struct TimestampedMsg {
    pub(crate) msg: ChatMessage,
    pub(crate) time: String,
}

pub(crate) struct App {
    pub(crate) messages: Vec<TimestampedMsg>,
    pub(crate) input: String,
    /// Cursor position as a **char index** (not byte index).
    /// Use `cursor_byte_pos()` to convert to byte offset for String operations.
    pub(crate) cursor_pos: usize,
    pub(crate) scroll_back: u16,
    /// When true, viewport stays pinned to the bottom (auto-scroll).
    /// Set to false when user scrolls up, true when they scroll back to bottom.
    pub(crate) scroll_pinned: bool,
    pub(crate) api_messages: Vec<Value>,
    pub(crate) streaming: bool,
    pub(crate) input_history: Vec<String>,
    pub(crate) history_index: Option<usize>,
    pub(crate) input_stash: String,
    /// Tab-completion cycle state for slash commands. `Some((prefix, index))`
    /// when the user is cycling through matches via repeated Tab; cleared
    /// on any non-Tab keypress. See input.rs::handle_tab_complete.
    pub(crate) tab_cycle: Option<(String, usize)>,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) total_input_tokens: u64,
    pub(crate) total_output_tokens: u64,
    pub(crate) total_cache_read_tokens: u64,
    pub(crate) total_cache_creation_tokens: u64,
    /// Most recent turn's actual context occupancy (what the API ingested
    /// this request): uncached input + cache read + cache creation. Unlike
    /// `total_*_tokens` which accumulate for cost tracking, this is reassigned
    /// every turn and reflects the current per-request context window use.
    /// Used by the context-usage bar in `draw.rs`.
    pub(crate) last_turn_context: u64,
    /// Context window size (in tokens) of the model that answered the most
    /// recent turn. Updated alongside `last_turn_context` so the bar's
    /// denominator adapts when users switch models mid-session. See
    /// `synaps_cli::models::context_window_for_model`.
    pub(crate) last_turn_context_window: u64,
    pub(crate) api_call_count: u32,
    pub(crate) session_cost: f64,
    pub(crate) session: Session,
    /// Cached wrapped+highlighted message lines.
    /// `None` means "stale — rebuild on next draw". `Some((w, lines))` means
    /// "valid at content width `w`". Collapses the old `(line_cache, cache_width, dirty)`
    /// trio into a single invariant-preserving field — impossible to desync.
    pub(crate) line_cache: Option<(usize, Vec<Line<'static>>)>,
    pub(crate) show_full_output: bool,
    pub(crate) logo_dismiss_t: Option<f64>,
    pub(crate) logo_build_t: Option<f64>,
    /// Previous rendered line count — used to stabilize scroll when not pinned
    pub(crate) last_line_count: usize,
    /// Active subagent status for the live panel
    pub(crate) subagents: Vec<SubagentState>,
    /// Counter for unique subagent IDs within a session
    /// Tracks when the current tool started executing (for elapsed time display)
    pub(crate) tool_start_time: Option<std::time::Instant>,
    /// Saved context from an aborted response — injected into the next user message
    pub(crate) abort_context: Option<String>,
    /// Message queued while streaming — auto-sent when current response finishes
    pub(crate) queued_message: Option<String>,
    /// Tracks paste state: snapshot of input before first paste, and total pasted char count
    pub(crate) input_before_paste: Option<String>,
    pub(crate) pasted_char_count: usize,
    /// Spinner frame counter (incremented on tick)
    pub(crate) spinner_frame: usize,
    /// Transient status text shown in the header bar (auto-cleared when streaming starts)
    pub(crate) status_text: Option<String>,
    /// GamblersDen child process — spawned by /gamba, killed when streaming finishes
    pub(crate) gamba_child: Option<std::process::Child>,
    /// Active settings modal state (Some while /settings is open).
    pub(crate) settings: Option<super::settings::SettingsState>,
    /// Active plugins modal state (Some while /plugins is open).
    pub(crate) plugins: Option<super::plugins::PluginsModalState>,
    /// Background compaction task — polled in the event loop so /compact doesn't block.
    pub(crate) compact_task: Option<tokio::task::JoinHandle<Result<String, synaps_cli::error::RuntimeError>>>,
}

pub(crate) const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];


#[derive(Clone)]
pub(crate) struct SubagentState {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) start_time: std::time::Instant,
    pub(crate) done: bool,
    pub(crate) duration_secs: Option<f64>,
}

impl App {
    pub(crate) fn new(session: Session) -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            scroll_back: 0,
            scroll_pinned: true,
            api_messages: Vec::new(),
            streaming: false,
            input_history: Vec::new(),
            history_index: None,
            input_stash: String::new(),
            tab_cycle: None,
            input_tokens: 0,
            output_tokens: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            last_turn_context: 0,
            last_turn_context_window: synaps_cli::models::context_window_for_model(
                synaps_cli::models::default_model(),
            ),
            api_call_count: 0,
            session_cost: 0.0,
            session,
            line_cache: None,
            show_full_output: false,
            logo_dismiss_t: None,
            logo_build_t: Some(0.0),
            last_line_count: 0,
            subagents: Vec::new(),
            tool_start_time: None,
            abort_context: None,
            queued_message: None,
            input_before_paste: None,
            pasted_char_count: 0,
            spinner_frame: 0,
            status_text: None,
            gamba_child: None,
            settings: None,
            plugins: None,
            compact_task: None,
        }
    }

    /// Restore SynapsCLI's TUI after casino (or failed spawn).
    /// Convert char-based cursor_pos to byte offset in self.input.
    pub(crate) fn cursor_byte_pos(&self) -> usize {
        self.input.char_indices()
            .nth(self.cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    /// Number of chars in self.input (for bounds checking cursor_pos).
    pub(crate) fn input_char_count(&self) -> usize {
        self.input.chars().count()
    }

    /// Calculate the number of visual lines the input needs, given an inner width.
    /// Returns (total_lines, cursor_row, cursor_col) for layout and cursor placement.
    pub(crate) fn input_wrap_info(&self, inner_width: u16) -> (u16, u16, u16) {
        use unicode_width::UnicodeWidthChar;
        let w = inner_width.max(1) as usize;
        // prefix "❯ " is 2 display columns (only on first line)
        let prefix_width: usize = 2;

        let mut row: u16 = 0;
        let mut col: usize = prefix_width;
        let mut cursor_row: u16 = 0;
        let mut cursor_col: u16 = prefix_width as u16;

        for (i, ch) in self.input.chars().enumerate() {
            if i == self.cursor_pos {
                cursor_row = row;
                cursor_col = col as u16;
            }
            if ch == '\n' {
                row += 1;
                col = prefix_width; // continuation lines also have 2-char indent
                continue;
            }
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if col + cw > w {
                row += 1;
                col = 0;
            }
            col += cw;
        }
        // If cursor is at the end
        if self.cursor_pos == self.input_char_count() {
            cursor_row = row;
            cursor_col = col as u16;
            // If cursor is exactly at the wrap boundary
            if col >= w {
                cursor_row += 1;
                cursor_col = 0;
            }
        }

        let total_lines = row + 1;
        (total_lines, cursor_row, cursor_col)
    }

    pub(crate) async fn save_session(&mut self) {
        if self.api_messages.is_empty() {
            return;
        }
        self.session.api_messages = self.api_messages.clone();
        self.session.total_input_tokens = self.total_input_tokens;
        self.session.total_output_tokens = self.total_output_tokens;
        self.session.session_cost = self.session_cost;
        self.session.abort_context = self.abort_context.clone();
        self.session.updated_at = chrono::Utc::now();
        self.session.auto_title();
        if let Err(e) = self.session.save().await {
            eprintln!("\x1b[31m[ERROR] Failed to save session: {}\x1b[0m", e);
        }
    }

    pub(crate) fn add_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        model: &str,
        context_window_override: Option<u64>,
    ) {
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
        self.total_input_tokens += input_tokens;
        self.total_output_tokens += output_tokens;
        self.total_cache_read_tokens += cache_read;
        self.total_cache_creation_tokens += cache_creation;
        // Per-turn context occupancy (bar numerator): what the API actually
        // ingested this request. Output tokens are generated, not ingested,
        // so they don't count toward current-window use. Reassigned, not accumulated.
        self.last_turn_context = input_tokens + cache_read + cache_creation;
        // Per-turn bar denominator — the context window of the model that
        // answered this turn. Tracked alongside so mid-session model swaps
        // (e.g. main thread Opus → subagent Sonnet) recalibrate the bar.
        // If the user configured an explicit context_window, honour it.
        self.last_turn_context_window = context_window_override
            .unwrap_or_else(|| synaps_cli::models::context_window_for_model(model));
        self.api_call_count += 1;
        // Pricing per million tokens (as of 2026-04, from platform.claude.com/docs/en/about-claude/pricing)
        // Opus 4.5+ = $5/$25, Sonnet 4+ = $3/$15, Haiku 4.5 = $1/$5, Haiku 3.5 = $0.80/$4
        // Note: output_tokens from the API includes adaptive thinking tokens —
        // these are invisible in the TUI but billed at the output rate.
        let (input_price, output_price) = match model {
            m if m.contains("opus") => (5.0, 25.0),
            m if m.contains("sonnet") => (3.0, 15.0),
            m if m.contains("haiku") => (1.0, 5.0), // Haiku 4.5 pricing
            _ => (3.0, 15.0), // default to sonnet pricing
        };
        // Cache reads bill at 0.1x input price; cache writes at 1.25x (5-min TTL)
        let cost = (input_tokens as f64 / 1_000_000.0) * input_price
                 + (cache_read as f64 / 1_000_000.0) * input_price * 0.1
                 + (cache_creation as f64 / 1_000_000.0) * input_price * 1.25
                 + (output_tokens as f64 / 1_000_000.0) * output_price;
        self.session_cost += cost;
    }

    pub(crate) fn push_msg(&mut self, msg: ChatMessage) {
        self.messages.push(TimestampedMsg {
            msg,
            time: Local::now().format("%H:%M").to_string(),
        });
        // Auto-scroll only when pinned to bottom
        if self.scroll_pinned {
            self.scroll_back = 0;
        }
        self.invalidate();
    }

    /// Mark the cached message lines stale — they'll be rebuilt on the next draw.
    /// Call this after any mutation that changes how `messages` renders.
    pub(crate) fn invalidate(&mut self) {
        self.line_cache = None;
    }

    /// Find the file extension from the ToolUse message preceding a ToolResult at index `idx`.
    pub(crate) fn find_preceding_read_extension(&self, idx: usize) -> String {
        // Walk backwards from idx to find the preceding ToolUse
        if idx == 0 { return String::new(); }
        for i in (0..idx).rev() {
            if let ChatMessage::ToolUse { ref tool_name, ref input } = self.messages[i].msg {
                if tool_name == "read" {
                    // Extract path from the JSON input
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(input) {
                        if let Some(path) = parsed["path"].as_str() {
                            // Get the extension
                            if let Some(ext) = std::path::Path::new(path).extension() {
                                return ext.to_string_lossy().to_string();
                            }
                        }
                    }
                }
                break; // Stop at first ToolUse regardless
            }
        }
        String::new()
    }

    /// Find the tool name from the ToolUse message preceding a ToolResult at index `idx`.
    pub(crate) fn find_preceding_tool_name(&self, idx: usize) -> Option<String> {
        if idx == 0 { return None; }
        for i in (0..idx).rev() {
            if let ChatMessage::ToolUse { ref tool_name, .. } = self.messages[i].msg {
                return Some(tool_name.clone());
            }
        }
        None
    }

    /// Capture all assistant output since the last user message as abort context.
    /// This gets injected into the next user message so the model knows what it
    /// was doing before the abort.
    pub(crate) fn capture_abort_context(&mut self) {
        let mut parts: Vec<String> = Vec::new();

        // Walk backwards from the end to find content since last user message
        for tmsg in self.messages.iter().rev() {
            match &tmsg.msg {
                ChatMessage::User(_) => break, // stop at the last user message
                ChatMessage::Thinking(t) => {
                    if !t.is_empty() {
                        // Truncate thinking to avoid bloating context
                        let preview: String = t.chars().take(500).collect();
                        parts.push(format!("[thinking]: {}", preview));
                    }
                }
                ChatMessage::Text(t) => {
                    if !t.is_empty() {
                        parts.push(format!("[response]: {}", t));
                    }
                }
                ChatMessage::ToolUse { tool_name, input } => {
                    // Truncate input to keep context lean
                    let input_preview: String = input.chars().take(200).collect();
                    parts.push(format!("[tool_use]: {} — {}", tool_name, input_preview));
                }
                ChatMessage::ToolResult { content, .. } => {
                    if !content.is_empty() {
                        let preview: String = content.chars().take(300).collect();
                        parts.push(format!("[tool_result]: {}", preview));
                    }
                }
                _ => {}
            }
        }

        if parts.is_empty() {
            self.abort_context = None;
            return;
        }

        parts.reverse(); // chronological order
        self.abort_context = Some(format!(
            "[ABORT CONTEXT — your previous response was interrupted. Here's what you completed before the abort:]\n\n{}\n\n[END ABORT CONTEXT — continue from where you left off or adjust based on the user's new message]",
            parts.join("\n")
        ));
    }

    pub(crate) fn history_up(&mut self) {
        if self.input_history.is_empty() { return; }
        match self.history_index {
            None => {
                self.input_stash = self.input.clone();
                self.history_index = Some(self.input_history.len() - 1);
            }
            Some(i) if i > 0 => {
                self.history_index = Some(i - 1);
            }
            _ => return,
        }
        if let Some(idx) = self.history_index {
            self.input = self.input_history[idx].clone();
            self.cursor_pos = self.input.chars().count();
        }
    }

    pub(crate) fn history_down(&mut self) {
        if let Some(i) = self.history_index {
            if i + 1 < self.input_history.len() {
                self.history_index = Some(i + 1);
                self.input = self.input_history[i + 1].clone();
            } else {
                self.history_index = None;
                self.input = self.input_stash.clone();
                self.input_stash.clear();
            }
            self.cursor_pos = self.input.chars().count();
        }
    }

    pub(crate) fn append_or_update_text(&mut self, text: &str) {
        if let Some(TimestampedMsg { msg: ChatMessage::Text(ref mut existing), .. }) = self.messages.last_mut() {
            existing.push_str(text);
        } else {
            self.push_msg(ChatMessage::Text(text.to_string()));
        }
        self.invalidate();
    }

    pub(crate) fn append_or_update_thinking(&mut self, text: &str) {
        if let Some(TimestampedMsg { msg: ChatMessage::Thinking(ref mut existing), .. }) = self.messages.last_mut() {
            existing.push_str(text);
        } else {
            self.push_msg(ChatMessage::Thinking(text.to_string()));
        }
        self.invalidate();
    }

    pub(crate) fn handle_theme_command(&mut self, arg: &str) {
        let descriptions: &[(&str, &str)] = &[
            ("default",        "cool teal on dark blue-gray"),
            ("neon-rain",      "cyberpunk hot pink + cyan"),
            ("amber",          "warm CRT retro terminal"),
            ("phosphor",       "green monochrome CRT"),
            ("solarized-dark", "Ethan Schoonover's classic"),
            ("blood",          "dark red, Doom/horror"),
            ("ocean",          "deep sea bioluminescence"),
            ("rose-pine",      "elegant muted purples/pinks"),
            ("nord",           "arctic frost blues"),
            ("dracula",        "purple/pink/cyan vibrant"),
            ("monokai",        "classic orange/pink/green"),
            ("gruvbox",        "warm earthy tones"),
            ("catppuccin",     "soft pastels, cozy dark"),
            ("tokyo-night",    "dark blue-purple, soft accents"),
            ("sunset",         "warm oranges/pinks dusk"),
            ("ice",            "frozen arctic pale blues"),
            ("forest",         "deep greens and browns"),
            ("lavender",       "rich purple/violet"),
        ];

        if arg.is_empty() {
            self.push_msg(ChatMessage::System("Available themes:".to_string()));
            for (name, desc) in descriptions {
                self.push_msg(ChatMessage::System(format!("  {:<15} — {}", name, desc)));
            }
            let themes_dir = synaps_cli::config::base_dir().join("themes");
            if let Ok(entries) = std::fs::read_dir(&themes_dir) {
                let mut custom: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| !descriptions.iter().any(|(d, _)| *d == n.as_str()))
                    .collect();
                custom.sort();
                for name in &custom {
                    self.push_msg(ChatMessage::System(format!("  {:<15} — custom", name)));
                }
            }
            self.push_msg(ChatMessage::System(String::new()));
            self.push_msg(ChatMessage::System("Usage: /theme <name> to set. Restart to apply.".to_string()));
        } else {
            let name = arg.trim();
            let is_valid = descriptions.iter().any(|(n, _)| *n == name)
                || synaps_cli::config::base_dir().join("themes").join(name).exists();

            if is_valid {
                match synaps_cli::config::write_config_value("theme", name) {
                    Ok(_) => {
                        let new_theme = super::theme::load_theme_by_name(name)
                            .unwrap_or_else(super::theme::Theme::default);
                        super::theme::set_theme(new_theme);
                        self.push_msg(ChatMessage::System(
                            format!("Theme applied: {}", name)
                        ));
                        self.invalidate();
                    }
                    Err(e) => {
                        self.push_msg(ChatMessage::Error(
                            format!("failed to write config: {}", e)
                        ));
                    }
                }
            } else {
                self.push_msg(ChatMessage::Error(
                    format!("unknown theme: '{}'. Use /theme to list available themes.", name)
                ));
            }
        }
    }

}
