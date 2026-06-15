use serde_json::Value;
use chrono::Local;
use synaps_cli::Session;
use synaps_cli::pricing::calculate_cost_optional_split;

/// Sentinel placeholder pushed into a `Thinking` block while the model is
/// deciding whether to think. Using ellipsis + zero-width space makes it
/// visually identical to "…" but never equal to real model output.
pub(crate) const THINKING_PLACEHOLDER: &str = "\u{2026}\u{200B}";

#[derive(Clone)]
pub(crate) enum ChatMessage {
    User(String),
    Thinking(String),
    Text(String),
    /// Streaming tool-use placeholder. `tool_id` lets the chat UI route
    /// subsequent input deltas and the final finalize event to *this*
    /// block when multiple tools run in parallel — without it, the
    /// "always update last message" hack misroutes deltas/results to
    /// whichever tool block happens to be most recent.
    ToolUseStart {
        tool_id: String,
        tool_name: String,
        partial_input: String,
    },
    ToolUse {
        tool_id: String,
        tool_name: String,
        input: String,
    },
    ToolResult {
        tool_id: String,
        content: String,
        elapsed_ms: Option<u64>,
    },
    Error(String),
    System(String),
    Event { source: String, severity: String, text: String },
}

pub(crate) struct TimestampedMsg {
    pub(crate) msg: ChatMessage,
    pub(crate) time: String,
}

/// Per-message render cache. Parallel to `App.messages`: each slot holds the
/// rendered `Vec<Line>` for that message. `flat` is their concatenation, which
/// is what downstream (draw/selection) consumes. The `width` at which these
/// were rendered is stored so stale entries can be detected on terminal resize.
pub(crate) struct LineCache {
    pub(crate) width: usize,
    /// Rendered lines per message — index parallel to App.messages.
    pub(crate) per_msg: Vec<Vec<ratatui::text::Line<'static>>>,
    /// Concatenation of per_msg; what downstream code consumes.
    pub(crate) flat: Vec<ratatui::text::Line<'static>>,
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
    /// Tab-completion cycle state for slash commands.
    /// `Some((prefix, index, matching_commands))` when the user is cycling
    /// through matches via repeated Tab; cleared on any non-Tab keypress.
    /// See input.rs::handle_tab_complete.
    pub(crate) tab_cycle: Option<(String, usize, Vec<String>)>,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) total_input_tokens: u64,
    pub(crate) total_output_tokens: u64,
    pub(crate) total_cache_read_tokens: u64,
    pub(crate) total_cache_creation_tokens: u64,
    /// Per-TTL-bucket accumulators for cache writes.  Populated by
    /// `add_usage` when the API returns the 5m/1h split in `SessionEvent::Usage`.
    /// Zero when no split has arrived (e.g. cache_ttl=default, no writes yet).
    pub(crate) total_cache_write_5m: u64,
    pub(crate) total_cache_write_1h: u64,
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
    pub(crate) agent_name: String,
    /// Cached wrapped+highlighted message lines.
    /// `None` means "stale — rebuild on next draw". `Some(cache)` means
    /// "valid at cache.width". Replaced the old `(usize, Vec<Line>)` tuple
    /// with a structured type that supports per-message incremental updates.
    pub(crate) line_cache: Option<LineCache>,
    /// Lowest message index whose rendered lines are stale. `None` = fully clean.
    /// Set to `Some(k)` to trigger partial re-render from message k on next draw.
    pub(crate) dirty_from: Option<usize>,
    pub(crate) needs_redraw: bool,
    /// When set, the next repaint bypasses the streaming redraw throttle.
    /// Set by user input (scroll/typing/cursor) so interaction stays instant
    /// even while the model is streaming; cleared after the paint.
    pub(crate) force_redraw: bool,
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
    /// Per-tool start times keyed by `tool_id`. Lets parallel tool calls
    /// each show their own elapsed-time on the result block, instead of
    /// sharing a single timer that the last-started tool clobbers.
    pub(crate) tool_start_times: std::collections::HashMap<String, std::time::Instant>,
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
    /// Active models router modal state (Some while /model or /models is open).
    pub(crate) models: Option<super::models::ModelsModalState>,
    /// Active /help find lightbox state.
    pub(crate) help_find: Option<synaps_cli::help::HelpFindState>,
    /// Background compaction task — polled in the event loop so /compact doesn't block.
    pub(crate) compact_task: Option<tokio::task::JoinHandle<Result<String, synaps_cli::error::RuntimeError>>>,
    /// Events buffered during streaming — injected into api_messages after stream completes
    pub(crate) pending_events: Vec<String>,
    /// Cached model ping results: "provider/model" -> (status, latency_ms).
    pub(crate) model_health: std::collections::HashMap<String, (synaps_cli::runtime::openai::ping::PingStatus, u64)>,
    /// Print ping results to chat as they arrive (set by /ping command).
    pub(crate) ping_print: bool,
    pub(crate) ping_pending: usize,
    /// Channel for receiving async ping results.
    pub(crate) ping_tx: tokio::sync::mpsc::UnboundedSender<(String, synaps_cli::runtime::openai::ping::PingStatus, u64)>,
    pub(crate) ping_rx: tokio::sync::mpsc::UnboundedReceiver<(String, synaps_cli::runtime::openai::ping::PingStatus, u64)>,
    /// Channel for receiving expanded model-list API results.
    pub(crate) model_list_tx: tokio::sync::mpsc::UnboundedSender<(String, Result<Vec<super::models::ExpandedModelEntry>, String>)>,
    pub(crate) model_list_rx: tokio::sync::mpsc::UnboundedReceiver<(String, Result<Vec<super::models::ExpandedModelEntry>, String>)>,
    /// Text selection state for the message area.
    /// Anchor is where the mouse was first pressed (col, row in terminal coords).
    /// End is the current drag position. Both are absolute terminal coordinates.
    pub(crate) selection_anchor: Option<(u16, u16)>,
    pub(crate) selection_end: Option<(u16, u16)>,
    /// The message area rect from the last draw, used by input.rs to map mouse
    /// coordinates to message content.
    pub(crate) msg_area_rect: Option<ratatui::layout::Rect>,
    /// The visible line range from the last draw: (start_line_index, end_line_index)
    /// into the line_cache, so we can extract text from screen coordinates.
    pub(crate) visible_line_range: Option<(usize, usize)>,
    /// Suppress paste events arriving shortly after a right-click copy/paste.
    /// Terminals that auto-paste on right-click generate a spurious Event::Paste
    /// immediately after MouseDown(Right). We suppress only within a short TTL
    /// window (~150ms) to avoid eating legitimate Ctrl+V pastes.
    pub(crate) suppress_paste_until: Option<std::time::Instant>,
    /// Active sidecar instances keyed by plugin id (manifest name).
    ///
    /// Phase 8 8B: replaces the legacy single `Option<SidecarUiState>` so
    /// multiple plugin-claimed sidecars can be hosted concurrently.
    pub(crate) sidecars: std::collections::HashMap<String, super::sidecar::SidecarUiState>,
    /// Generic extension-provided active tasks rendered in the sticky progress area.
    /// Stored behind `Arc` so the per-frame snapshot is a refcount bump, not a deep clone.
    pub(crate) active_tasks: std::sync::Arc<synaps_cli::extensions::active_tasks::ActiveTasks>,
    /// Overlay toast provider used by core and extension-adjacent features.
    pub(crate) toasts: super::toast::ToastProvider,
    /// Channel for async extension loader progress events.
    pub(crate) extension_loader_rx: tokio::sync::mpsc::UnboundedReceiver<synaps_cli::extensions::loader::ExtensionLoaderEvent>,
    pub(crate) extension_loader_tx: tokio::sync::mpsc::UnboundedSender<synaps_cli::extensions::loader::ExtensionLoaderEvent>,
    pub(crate) extension_loader_running: bool,
    /// Channel for receiving widget events from background extension notification watchers.
    pub(crate) widget_rx: tokio::sync::mpsc::UnboundedReceiver<synaps_cli::extensions::widgets::ExtensionWidgetEvent>,
    pub(crate) widget_tx: tokio::sync::mpsc::UnboundedSender<synaps_cli::extensions::widgets::ExtensionWidgetEvent>,
    /// Live keybind registry — held so /settings can hot-swap plugin toggle keys.
    pub(crate) keybinds: Option<std::sync::Arc<std::sync::RwLock<synaps_cli::skills::keybinds::KeybindRegistry>>>,
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
        let (ping_tx_init, ping_rx_init) = tokio::sync::mpsc::unbounded_channel();
        let (model_list_tx_init, model_list_rx_init) = tokio::sync::mpsc::unbounded_channel();
        let (extension_loader_tx_init, extension_loader_rx_init) = tokio::sync::mpsc::unbounded_channel();
        let (widget_tx_init, widget_rx_init) = tokio::sync::mpsc::unbounded_channel();
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
            total_cache_write_5m: 0,
            total_cache_write_1h: 0,
            last_turn_context: 0,
            last_turn_context_window: synaps_cli::models::context_window_for_model(
                synaps_cli::models::default_model(),
            ),
            api_call_count: 0,
            session_cost: 0.0,
            session,
            agent_name: synaps_cli::config::load_config()
                .agent_name
                .unwrap_or_else(|| "agent".to_string()),
            line_cache: None,
            dirty_from: None,
            needs_redraw: true,
            force_redraw: false,
            show_full_output: false,
            logo_dismiss_t: None,
            logo_build_t: Some(0.0),
            last_line_count: 0,
            subagents: Vec::new(),
            tool_start_time: None,
            tool_start_times: std::collections::HashMap::new(),
            abort_context: None,
            queued_message: None,
            input_before_paste: None,
            pasted_char_count: 0,
            spinner_frame: 0,
            status_text: None,
            gamba_child: None,
            settings: None,
            plugins: None,
            models: None,
            help_find: None,
            compact_task: None,
            pending_events: Vec::new(),
            model_health: std::collections::HashMap::new(),
            ping_print: false,
            ping_pending: 0,
            ping_tx: ping_tx_init,
            ping_rx: ping_rx_init,
            model_list_tx: model_list_tx_init,
            model_list_rx: model_list_rx_init,
            selection_anchor: None,
            selection_end: None,
            msg_area_rect: None,
            visible_line_range: None,
            suppress_paste_until: None,
            sidecars: std::collections::HashMap::new(),
            active_tasks: std::sync::Arc::new(synaps_cli::extensions::active_tasks::ActiveTasks::new()),
            toasts: super::toast::ToastProvider::new(),
            extension_loader_rx: extension_loader_rx_init,
            extension_loader_tx: extension_loader_tx_init,
            extension_loader_running: false,
            widget_rx: widget_rx_init,
            widget_tx: widget_tx_init,
            keybinds: None,
        }
    }
    /// Build the text shown in the chat transcript for a submitted user message.
    /// Large pasted payloads are collapsed to a label, while any text typed before
    /// or after the pasted range remains visible.
    pub(crate) fn user_display_text_for_submission(&self, input: &str) -> String {
        if self.pasted_char_count == 0 {
            return input.to_string();
        }

        let before_paste = self.input_before_paste.as_deref().unwrap_or("");
        let before_chars = before_paste.chars().count();
        let total_chars = input.chars().count();
        let paste_chars = self.pasted_char_count.min(total_chars.saturating_sub(before_chars));
        let after_chars = total_chars.saturating_sub(before_chars + paste_chars);

        let paste_byte_start = input
            .char_indices()
            .nth(before_chars)
            .map(|(i, _)| i)
            .unwrap_or(input.len());
        let paste_byte_end = input
            .char_indices()
            .nth(before_chars + paste_chars)
            .map(|(i, _)| i)
            .unwrap_or(input.len());

        let pasted = &input[paste_byte_start..paste_byte_end];
        let after_paste = if after_chars == 0 {
            ""
        } else {
            &input[paste_byte_end..]
        };

        let line_count = pasted.lines().count();
        let paste_label = if line_count > 1 {
            format!("[Pasted {} lines]", line_count)
        } else {
            format!("[Pasted {} chars]", paste_chars)
        };

        match (before_paste.is_empty(), after_paste.is_empty()) {
            (true, true) => paste_label,
            (false, true) => format!("{} {}", before_paste.trim(), paste_label),
            (true, false) => format!("{} {}", paste_label, after_paste.trim()),
            (false, false) => format!(
                "{} {} {}",
                before_paste.trim(),
                paste_label,
                after_paste.trim()
            ),
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

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        cache_creation_5m: Option<u64>,
        cache_creation_1h: Option<u64>,
        model: &str,
        context_window_override: Option<u64>,
    ) {
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
        self.total_input_tokens += input_tokens;
        self.total_output_tokens += output_tokens;
        self.total_cache_read_tokens += cache_read;
        self.total_cache_creation_tokens += cache_creation;
        // Accumulate per-TTL-bucket write totals for /stats and footer display.
        if let Some(c5) = cache_creation_5m { self.total_cache_write_5m += c5; }
        if let Some(c1) = cache_creation_1h { self.total_cache_write_1h += c1; }
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
        // Delegate cost calculation to the single source of truth in `pricing`.
        // Split-aware: 1h cache writes bill at 2.0x when the TTL split arrived.
        self.session_cost += calculate_cost_optional_split(
            model, input_tokens, output_tokens, cache_read,
            cache_creation, cache_creation_5m, cache_creation_1h,
        );
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
        // New tail message — mark last slot dirty (append to per_msg on next rebuild).
        self.invalidate_last();
    }

    /// Insert a freshly created `ToolResult` directly beneath its matching
    /// `ToolUse` / `ToolUseStart` block (matched by `tool_id`) so parallel tool
    /// calls render as **input → its output** pairs, instead of all inputs
    /// stacked then all outputs stacked. Falls back to appending at the end
    /// when no matching tool_use exists (legacy providers without tool_ids).
    pub(crate) fn push_tool_result(&mut self, tool_id: String, content: String, elapsed_ms: Option<u64>) {
        let use_idx = if tool_id.is_empty() {
            None
        } else {
            // Invariant: each tool_id should appear at most once. Assert in
            // debug builds so duplicate IDs surface immediately.
            debug_assert!(
                self.messages.iter().filter(|m| matches!(
                    &m.msg,
                    ChatMessage::ToolUse { tool_id: tid, .. }
                    | ChatMessage::ToolUseStart { tool_id: tid, .. }
                        if tid == &tool_id
                )).count() <= 1,
                "push_tool_result: duplicate ToolUse/ToolUseStart for tool_id={tool_id:?}"
            );
            self.messages.iter().position(|m| matches!(
                &m.msg,
                ChatMessage::ToolUse { tool_id: tid, .. }
                | ChatMessage::ToolUseStart { tool_id: tid, .. }
                    if tid == &tool_id
            ))
        };
        let msg = ChatMessage::ToolResult { tool_id, content, elapsed_ms };
        match use_idx {
            Some(i) => {
                let at = (i + 1).min(self.messages.len());
                self.messages.insert(at, TimestampedMsg {
                    msg,
                    time: Local::now().format("%H:%M").to_string(),
                });
                if self.scroll_pinned {
                    self.scroll_back = 0;
                }
                // Insert mid-list — invalidate_from(at) so draw re-renders from insert point.
                self.invalidate_from(at);
            }
            None => self.push_msg(msg),
        }
    }
    ///
    /// `render_lines` markdown-renders + syntax-highlights EVERY display message
    /// on the first frame. On `--continue` of a long session (hundreds of
    /// messages) that made boot crawl. This keeps only the most recent `cap`
    /// display messages and prepends a notice — the FULL history is untouched in
    /// `api_messages`, so the model still sees everything; only the visible
    /// scrollback is trimmed. (Proper fix is viewport virtualization — #98.)
    pub(crate) fn cap_resumed_display(&mut self, cap: usize) {
        if self.messages.len() <= cap {
            return;
        }
        let omitted = self.messages.len() - cap;
        self.messages.drain(0..omitted);
        self.messages.insert(
            0,
            TimestampedMsg {
                msg: ChatMessage::System(format!(
                    "… {omitted} earlier message(s) hidden to speed resume — full history is still in the model's context"
                )),
                time: Local::now().format("%H:%M").to_string(),
            },
        );
    }

    /// Mark the cached message lines stale — they'll be rebuilt on the next draw.
    /// Call this after any mutation that changes how `messages` renders.
    /// Use for structural changes (theme, width, message list reshuffle). For
    /// streaming deltas prefer `invalidate_last()` which is O(1).
    pub(crate) fn invalidate(&mut self) {
        self.line_cache = None;
        self.dirty_from = None;
        self.needs_redraw = true;
    }

    /// Mark messages from index `idx` onwards as dirty (cheapest granularity).
    /// Coalesces with any existing dirty_from by taking the minimum.
    pub(crate) fn invalidate_from(&mut self, idx: usize) {
        self.dirty_from = Some(match self.dirty_from {
            Some(k) => k.min(idx),
            None => idx,
        });
        self.needs_redraw = true;
    }

    /// Mark only the tail message dirty. O(1) during streaming.
    pub(crate) fn invalidate_last(&mut self) {
        self.invalidate_from(self.messages.len().saturating_sub(1));
    }

    /// Request a redraw without invalidating the line cache (e.g. for
    /// panel-only changes like spinner/timer updates, scroll, cursor blink).
    pub(crate) fn request_redraw(&mut self) {
        self.needs_redraw = true;
    }

    /// Request an immediate repaint that bypasses the streaming redraw throttle.
    /// Use for user-driven changes (scroll, typing, cursor, paste, resize) that
    /// must feel instant even while the model is streaming. Streaming text
    /// deltas use the throttled `request_redraw` / `invalidate` paths.
    pub(crate) fn request_immediate_redraw(&mut self) {
        self.needs_redraw = true;
        self.force_redraw = true;
    }

    /// Advance spinner/animation state.
    ///
    /// Returns true only when text generated from `render_lines` may have changed
    /// and the cached message lines must be rebuilt. Full-screen overlays and
    /// header/panel spinners are redrawn from current state without touching the
    /// message cache, avoiding unnecessary whole-terminal clears on every frame.
    pub(crate) fn advance_animations(&mut self) -> bool {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
        if self.spinner_frame % 3 != 0 {
            return false;
        }

        self.render_lines_uses_spinner()
    }

    fn render_lines_uses_spinner(&self) -> bool {
        self.messages.iter().enumerate().any(|(idx, msg)| match &msg.msg {
            ChatMessage::Thinking(text) => text == THINKING_PLACEHOLDER,
            ChatMessage::ToolUseStart { .. } => true,
            ChatMessage::ToolUse { .. } => {
                idx == self.messages.len().saturating_sub(1) && self.tool_start_time.is_some()
            }
            ChatMessage::ToolResult { .. } => self.is_active_tool_result(idx),
            _ => false,
        })
    }

    /// True when a full terminal clear is needed before an animated redraw.
    ///
    /// The message pane is already cleared locally in `draw()` before rendering
    /// the transcript, and rendered lines are clamped to terminal display width.
    /// Calling `terminal.clear()` on streaming animation frames repaints the
    /// whole alternate screen and causes visible flicker, so animation ticks
    /// should not request a full-screen clear.
    pub(crate) fn needs_clear_for_animation_redraw(&self) -> bool {
        false
    }

    /// Returns true when the chat message at `idx` is the tool result currently
    /// being streamed/executed. Completed historical tool results must render as
    /// done even while a later tool call is active.
    pub(crate) fn is_active_tool_result(&self, idx: usize) -> bool {
        if self.tool_start_time.is_none() {
            return false;
        }
        match self.messages.get(idx).map(|m| &m.msg) {
            Some(ChatMessage::ToolResult { tool_id, elapsed_ms: None, .. }) => {
                self.tool_start_times.contains_key(tool_id)
            }
            _ => false,
        }
    }

    /// Find the file extension from the ToolUse message preceding a ToolResult at index `idx`.
    pub(crate) fn find_preceding_read_extension(&self, idx: usize) -> String {
        // Prefer matching by tool_id when the result carries one — under
        // parallel tool calls a `ToolResult` may not be positionally
        // adjacent to its matching `ToolUse`.
        let target_id: Option<String> = match self.messages.get(idx).map(|m| &m.msg) {
            Some(ChatMessage::ToolResult { tool_id, .. }) if !tool_id.is_empty() => Some(tool_id.clone()),
            _ => None,
        };
        if let Some(id) = target_id {
            for m in self.messages.iter() {
                if let ChatMessage::ToolUse { tool_id, tool_name, input } = &m.msg {
                    if tool_id == &id && tool_name == "read" {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(input) {
                            if let Some(path) = parsed["path"].as_str() {
                                if let Some(ext) = std::path::Path::new(path).extension() {
                                    return ext.to_string_lossy().to_string();
                                }
                            }
                        }
                        return String::new();
                    }
                }
            }
        }
        // Fallback: walk backwards from idx to find the preceding ToolUse
        if idx == 0 { return String::new(); }
        for i in (0..idx).rev() {
            if let ChatMessage::ToolUse { ref tool_name, ref input, .. } = self.messages[i].msg {
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
        // Prefer matching by tool_id when the result carries one — this
        // is the only way to render parallel-tool outputs correctly,
        // since results may not be positionally adjacent to their
        // matching tool_use.
        if let Some(ChatMessage::ToolResult { tool_id, .. }) = self.messages.get(idx).map(|m| &m.msg) {
            if !tool_id.is_empty() {
                for m in self.messages.iter() {
                    match &m.msg {
                        ChatMessage::ToolUse { tool_id: tid, tool_name, .. }
                        | ChatMessage::ToolUseStart { tool_id: tid, tool_name, .. }
                            if tid == tool_id =>
                        {
                            return Some(tool_name.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
        if idx == 0 { return None; }
        for i in (0..idx).rev() {
            if let ChatMessage::ToolUse { ref tool_name, .. } = self.messages[i].msg {
                return Some(tool_name.clone());
            }
            if let ChatMessage::ToolUseStart { ref tool_name, .. } = self.messages[i].msg {
                return Some(tool_name.clone());
            }
        }
        None
    }

    // ── Tool-event routing ──────────────────────────────────────────────
    //
    // Stream events arrive interleaved when the model fans out parallel
    // tool calls. The chat UI keeps a flat `Vec<ChatMessage>`, so to keep
    // each on-screen tool block correct we must route every delta /
    // finalize / result event back to the block whose `tool_id` matches.
    //
    // The earlier "always update last message" approach worked for
    // sequential tool calls but corrupts state under parallelism — input
    // deltas from tool A would land on tool B's `ToolUseStart`, and the
    // first arriving result would be silently overwritten by the second.

    /// Locate the index of a `ToolUseStart` block with this `tool_id`.
    pub(crate) fn find_tool_use_start_idx(&self, tool_id: &str) -> Option<usize> {
        self.messages.iter().rposition(|m| matches!(
            &m.msg,
            ChatMessage::ToolUseStart { tool_id: tid, .. } if tid == tool_id
        ))
    }

    /// Locate the latest `ToolResult` block for this `tool_id`.
    pub(crate) fn find_tool_result_idx(&self, tool_id: &str) -> Option<usize> {
        self.messages.iter().rposition(|m| matches!(
            &m.msg,
            ChatMessage::ToolResult { tool_id: tid, .. } if tid == tool_id
        ))
    }

    /// Begin streaming a new tool call. Records start time per-tool so
    /// elapsed-ms is correct under parallel execution.
    pub(crate) fn on_tool_use_start(&mut self, tool_id: String, tool_name: String) {
        self.drop_empty_thinking();
        let now = std::time::Instant::now();
        self.tool_start_time = Some(now);
        if !tool_id.is_empty() {
            self.tool_start_times.insert(tool_id.clone(), now);
        }
        self.push_msg(ChatMessage::ToolUseStart {
            tool_id,
            tool_name,
            partial_input: String::new(),
        });
    }

    /// Append a chunk of the tool's input JSON to the matching
    /// `ToolUseStart` block. Falls back to "last ToolUseStart" only when
    /// the event lacks a tool_id (legacy paths).
    pub(crate) fn on_tool_use_delta(&mut self, tool_id: &str, delta: &str) {
        let target_idx = if !tool_id.is_empty() {
            self.find_tool_use_start_idx(tool_id)
        } else {
            self.messages.iter().rposition(|m| matches!(&m.msg, ChatMessage::ToolUseStart { .. }))
        };
        if let Some(idx) = target_idx {
            if let ChatMessage::ToolUseStart { ref mut partial_input, .. } = self.messages[idx].msg {
                partial_input.push_str(delta);
                self.invalidate();
            }
        }
    }

    /// Finalize a streaming tool call. Replaces the matching
    /// `ToolUseStart` in place — keeping its position so on-screen order
    /// matches the order the model emitted the calls.
    pub(crate) fn on_tool_use_finalized(&mut self, tool_id: String, tool_name: String, input_str: String) {
        self.drop_empty_thinking();
        // Track start time even if we never saw a ToolUseStart (some
        // providers go straight to a finalized tool_use).
        if !tool_id.is_empty() {
            self.tool_start_times.entry(tool_id.clone()).or_insert_with(std::time::Instant::now);
        }
        self.tool_start_time = Some(std::time::Instant::now());

        if let Some(idx) = self.find_tool_use_start_idx(&tool_id) {
            self.messages[idx].msg = ChatMessage::ToolUse { tool_id, tool_name, input: input_str };
            self.invalidate();
            return;
        }
        // No matching start (e.g. provider only emits finalized blocks) —
        // append a new finalized block at the end.
        self.push_msg(ChatMessage::ToolUse { tool_id, tool_name, input: input_str });
    }

    /// Stream a chunk of tool output. Appends to the matching
    /// `ToolResult` if one exists, otherwise creates a new placeholder.
    pub(crate) fn on_tool_result_delta(&mut self, tool_id: String, delta: String) {
        if let Some(idx) = self.find_tool_result_idx(&tool_id) {
            if let ChatMessage::ToolResult { ref mut content, elapsed_ms, .. } = self.messages[idx].msg {
                if elapsed_ms.is_none() {
                    content.push_str(&delta);
                    self.invalidate();
                    return;
                }
            }
        }
        self.push_tool_result(tool_id, delta, None);
    }

    /// Finalize a tool result. Replaces any in-flight `ToolResult` for
    /// this `tool_id` (including a delta-buffered one) and stamps the
    /// elapsed time using the per-tool start time.
    pub(crate) fn on_tool_result(&mut self, tool_id: String, result: String) {
        let elapsed = self
            .tool_start_times
            .remove(&tool_id)
            .map(|t| t.elapsed().as_millis() as u64);
        // Clear the shared "active tool" timer once the *latest* tool
        // finishes — otherwise the bash trace animation lingers.
        if self.tool_start_times.is_empty() {
            self.tool_start_time = None;
        }

        if let Some(idx) = self.find_tool_result_idx(&tool_id) {
            if let ChatMessage::ToolResult { ref mut content, elapsed_ms, .. } = self.messages[idx].msg {
                if elapsed_ms.is_none() {
                    *content = result;
                    self.messages[idx].msg = ChatMessage::ToolResult {
                        tool_id,
                        content: std::mem::take(content),
                        elapsed_ms: elapsed,
                    };
                    self.invalidate();
                    return;
                }
            }
        }
        self.push_tool_result(tool_id, result, elapsed);
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
                ChatMessage::Thinking(t)
                    if !t.is_empty() => {
                        // Truncate thinking to avoid bloating context
                        let preview: String = t.chars().take(500).collect();
                        parts.push(format!("[thinking]: {}", preview));
                    }
                ChatMessage::Text(t)
                    if !t.is_empty() => {
                        parts.push(format!("[response]: {}", t));
                    }
                ChatMessage::ToolUse { tool_name, input, .. } => {
                    // Truncate input to keep context lean
                    let input_preview: String = input.chars().take(200).collect();
                    parts.push(format!("[tool_use]: {} — {}", tool_name, input_preview));
                }
                ChatMessage::ToolResult { content, .. }
                    if !content.is_empty() => {
                        let preview: String = content.chars().take(300).collect();
                        parts.push(format!("[tool_result]: {}", preview));
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

    /// Returns true if there is an active text selection in the message area.
    pub(crate) fn has_selection(&self) -> bool {
        self.selection_anchor.is_some() && self.selection_end.is_some()
    }

    /// Clear the current text selection.
    pub(crate) fn clear_selection(&mut self) {
        self.selection_anchor = None;
        self.selection_end = None;
    }

    /// Get the normalized selection range: (start_col, start_row, end_col, end_row)
    /// where start <= end in reading order. Returns None if no selection.
    pub(crate) fn selection_range(&self) -> Option<(u16, u16, u16, u16)> {
        let (ac, ar) = self.selection_anchor?;
        let (ec, er) = self.selection_end?;
        // Normalize: start is the earlier position in reading order
        if ar < er || (ar == er && ac <= ec) {
            Some((ac, ar, ec, er))
        } else {
            Some((ec, er, ac, ar))
        }
    }

    /// Rendering margin used in render.rs for message continuation lines.
    /// 3-char margin + 2-char content indent = 5 chars total.
    const MSG_LINE_INDENT: &'static str = "     ";

    /// Extract the selected text from the visible line cache.
    /// Uses msg_area_rect and visible_line_range to map terminal coordinates
    /// back to line content. msg_area_rect stores the inner content rect
    /// (after borders/padding), so no offset arithmetic is needed here.
    pub(crate) fn selected_text(&self) -> Option<String> {
        let (sc, sr, ec, er) = self.selection_range()?;
        let rect = self.msg_area_rect?;
        let (vis_start, vis_end) = self.visible_line_range?;
        let all_lines = &self.line_cache.as_ref()?.flat;

        let content_x = rect.x;
        let content_y = rect.y;
        let content_h = rect.height;

        // Convert terminal y-coordinates to line indices
        let mut result = String::new();
        for term_y in sr..=er {
            if term_y < content_y || term_y >= content_y + content_h {
                continue;
            }
            let line_offset = (term_y - content_y) as usize;
            let line_idx = vis_start + line_offset;
            if line_idx >= vis_end || line_idx >= all_lines.len() {
                continue;
            }
            let line = &all_lines[line_idx];
            // Extract text from the line spans
            let full_text: String = line.spans.iter()
                .map(|s| s.content.as_ref())
                .collect();

            // Determine character range on this line
            let line_start_col = if term_y == sr {
                (sc.saturating_sub(content_x)) as usize
            } else {
                0
            };
            let line_end_col = if term_y == er {
                (ec.saturating_sub(content_x)) as usize
            } else {
                full_text.len()
            };

            let chars: Vec<char> = full_text.chars().collect();
            let start = line_start_col.min(chars.len());
            let end = line_end_col.min(chars.len());
            if start < end {
                let selected: String = chars[start..end].iter().collect();
                let trimmed = selected.trim_end();
                let trimmed = if result.is_empty() {
                    trimmed.trim_start()
                } else {
                    trimmed.strip_prefix(Self::MSG_LINE_INDENT).unwrap_or(trimmed)
                };
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(trimmed);
            }
        }

        if result.is_empty() { None } else { Some(result) }
    }

    pub(crate) fn append_or_update_text(&mut self, text: &str) {
        // Model produced real output — clear any empty thinking placeholder
        // so its spinner stops.
        self.drop_empty_thinking();
        if let Some(TimestampedMsg { msg: ChatMessage::Text(ref mut existing), .. }) = self.messages.last_mut() {
            existing.push_str(text);
        } else {
            self.push_msg(ChatMessage::Text(text.to_string()));
        }
        self.invalidate_last();
    }

    pub(crate) fn append_or_update_thinking(&mut self, text: &str) {
        if let Some(TimestampedMsg { msg: ChatMessage::Thinking(ref mut existing), .. }) = self.messages.last_mut() {
            // First real delta replaces the sentinel placeholder rather than
            // appending to it (otherwise content reads "…​<thinking>").
            if existing == THINKING_PLACEHOLDER {
                *existing = text.to_string();
            } else {
                existing.push_str(text);
            }
        } else {
            self.push_msg(ChatMessage::Thinking(text.to_string()));
        }
        self.invalidate_last();
    }

    /// Remove a trailing thinking block that never received content — the
    /// sentinel placeholder, or one left empty. Called when the model starts
    /// producing real output or the turn ends, so the thinking spinner can't
    /// run forever on an empty thinking step.
    ///
    /// Scans backward past trailing System/Notice messages so a placeholder
    /// stranded mid-list (e.g. when a Notice arrives on top of it) is still
    /// found and removed (FIX 4).
    pub(crate) fn drop_empty_thinking(&mut self) {
        // Walk from the tail, skipping System messages, to find the first
        // non-System message. If it's an empty/placeholder Thinking, drop it.
        let candidate_idx = self.messages.iter().rposition(|m| {
            !matches!(&m.msg, ChatMessage::System(_))
        });
        if let Some(idx) = candidate_idx {
            if let ChatMessage::Thinking(t) = &self.messages[idx].msg {
                if t == THINKING_PLACEHOLDER || t.is_empty() {
                    self.messages.remove(idx);
                    // Structural change (remove) — full invalidate to resync per_msg lengths.
                    self.invalidate();
                }
            }
        }
    }

    pub(crate) fn handle_theme_command(&mut self, arg: &str) {
        let descriptions: &[(&str, &str)] = &[
            ("default",        "cool teal on dark blue-gray"),
            ("night-city",     "premium neon-noir — cyberpunk/blade runner"),
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
                            .unwrap_or_default();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(Session::new("test-model", "low", None))
    }

    #[test]
    fn empty_thinking_placeholder_is_dropped_when_agent_moves_on() {
        // Spinner is driven by a Thinking(THINKING_PLACEHOLDER) block. If thinking stays
        // empty, producing text/tools must remove it so the spinner stops.
        let mut app = test_app();

        // Case 1: text follows empty thinking.
        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
        assert!(app.render_lines_uses_spinner(), "placeholder should animate while present");
        app.append_or_update_text("here is the answer");
        assert!(
            !matches!(app.messages.last().map(|m| &m.msg), Some(ChatMessage::Thinking(_))),
            "empty thinking must be gone once text arrives"
        );
        assert!(!app.render_lines_uses_spinner(), "spinner must stop after agent moves on");

        // Case 2: a tool follows empty thinking.
        let mut app = test_app();
        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
        app.on_tool_use_start("t1".to_string(), "bash".to_string());
        let thinking_count = app
            .messages
            .iter()
            .filter(|m| matches!(m.msg, ChatMessage::Thinking(_)))
            .count();
        assert_eq!(thinking_count, 0, "empty thinking must be gone once a tool runs");

        // Case 3: real thinking content is preserved.
        let mut app = test_app();

        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
        app.append_or_update_thinking("actually reasoning");
        app.append_or_update_text("done");
        assert!(
            app.messages.iter().any(|m| matches!(&m.msg, ChatMessage::Thinking(t) if t == "actually reasoning")),
            "non-empty thinking must survive and not keep the … prefix"
        );
    }

    #[test]
    fn parallel_tool_results_pair_with_their_inputs() {
        let mut app = test_app();
        // Model fans out four tool calls — all inputs arrive first.
        for id in ["t1", "t2", "t3", "t4"] {
            app.on_tool_use_finalized(id.to_string(), "bash".to_string(), "{}".to_string());
        }
        // Results return out of completion order; each must slot under its input.
        for id in ["t2", "t1", "t4", "t3"] {
            app.on_tool_result(id.to_string(), format!("out-{id}"));
        }
        let seq: Vec<(String, bool)> = app
            .messages
            .iter()
            .filter_map(|m| match &m.msg {
                ChatMessage::ToolUse { tool_id, .. } => Some((tool_id.clone(), false)),
                ChatMessage::ToolResult { tool_id, .. } => Some((tool_id.clone(), true)),
                _ => None,
            })
            .collect();
        let expected = vec![
            ("t1".to_string(), false), ("t1".to_string(), true),
            ("t2".to_string(), false), ("t2".to_string(), true),
            ("t3".to_string(), false), ("t3".to_string(), true),
            ("t4".to_string(), false), ("t4".to_string(), true),
        ];
        assert_eq!(seq, expected, "parallel tool calls must render as input→output pairs");
    }

    #[test]
    fn tool_block_has_gutter_and_background() {
        let mut app = test_app();
        app.on_tool_use_finalized("t1".to_string(), "bash".to_string(), "{}".to_string());
        app.on_tool_result("t1".to_string(), "hello\nworld".to_string());
        let lines = app.render_lines(80);
        // No borders / shade glyphs.
        for l in &lines {
            let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            assert!(
                !s.chars().any(|c| matches!(c,
                    '\u{256D}' | '\u{256E}' | '\u{2570}' | '\u{256F}' | '\u{2502}'
                    | '\u{2591}' | '\u{2592}' | '\u{2593}')),
                "no borders or shade glyphs: {s:?}"
            );
        }
        // Panel lines carry a gutter bar ▎ and a unified background.
        let panel_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains('\u{258E}')))
            .expect("a panel line with a gutter");
        assert!(
            panel_line.spans.iter().any(|s| s.style.bg.is_some()),
            "panel cells (incl. text) must share a background"
        );
        // The inset left margin stays transparent.
        if let Some(first) = panel_line.spans.first() {
            if !first.content.is_empty() && first.content.chars().all(|c| c == ' ') {
                assert!(first.style.bg.is_none(), "left margin must stay transparent");
            }
        }
    }

    #[test]
    fn active_tool_result_is_only_latest_incomplete_result() {
        let mut app = test_app();
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "call_1".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app.push_msg(ChatMessage::ToolResult {
            tool_id: "call_1".to_string(),
            content: "first output".to_string(),
            elapsed_ms: None,
        });
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "call_2".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app.push_msg(ChatMessage::ToolResult {
            tool_id: "call_2".to_string(),
            content: "second output".to_string(),
            elapsed_ms: None,
        });
        // Only call_2 is still in tool_start_times (call_1 is done)
        app.tool_start_time = Some(std::time::Instant::now());
        app.tool_start_times.insert("call_2".to_string(), std::time::Instant::now());

        assert!(!app.is_active_tool_result(1), "completed historical result (call_1, not in tool_start_times) must render done");
        assert!(app.is_active_tool_result(3), "latest in-flight result (call_2, in tool_start_times) must be active");

        // Bonus: with BOTH in-flight (parallel), BOTH must be active
        let mut app2 = test_app();
        app2.push_msg(ChatMessage::ToolUse {
            tool_id: "p1".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app2.push_msg(ChatMessage::ToolResult {
            tool_id: "p1".to_string(),
            content: "".to_string(),
            elapsed_ms: None,
        });
        app2.push_msg(ChatMessage::ToolUse {
            tool_id: "p2".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app2.push_msg(ChatMessage::ToolResult {
            tool_id: "p2".to_string(),
            content: "".to_string(),
            elapsed_ms: None,
        });
        app2.tool_start_time = Some(std::time::Instant::now());
        app2.tool_start_times.insert("p1".to_string(), std::time::Instant::now());
        app2.tool_start_times.insert("p2".to_string(), std::time::Instant::now());
        assert!(app2.is_active_tool_result(1), "parallel in-flight p1 mid-vec must be active");
        assert!(app2.is_active_tool_result(3), "parallel in-flight p2 last must be active");
    }

    #[test]
    fn completed_latest_tool_result_is_not_active() {
        let mut app = test_app();
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "call_1".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app.push_msg(ChatMessage::ToolResult {
            tool_id: "call_1".to_string(),
            content: "done".to_string(),
            elapsed_ms: Some(25),
        });
        app.tool_start_time = Some(std::time::Instant::now());

        assert!(!app.is_active_tool_result(1));
    }

    #[test]
    fn animation_tick_for_subagent_panel_does_not_invalidate_message_cache() {
        let mut app = test_app();
        app.push_msg(ChatMessage::System("stable transcript".to_string()));
        app.line_cache = Some({
            let w = 80;
            let per_msg: Vec<Vec<ratatui::text::Line<'static>>> = (0..app.messages.len()).map(|i| app.render_message_lines(i, w)).collect();
            let flat: Vec<ratatui::text::Line<'static>> = per_msg.iter().flatten().cloned().collect();
            LineCache { width: w, per_msg, flat }
        });
        app.subagents.push(SubagentState {
            id: 1,
            name: "tester".to_string(),
            status: "running".to_string(),
            start_time: std::time::Instant::now(),
            done: false,
            duration_secs: None,
        });
        app.spinner_frame = 2;

        let invalidate_messages = app.advance_animations();

        assert!(!invalidate_messages, "subagent panel spinner redraw must not rebuild message cache");
        assert!(!app.needs_clear_for_animation_redraw(), "subagent-only animation must not force terminal.clear flicker");
        assert!(app.line_cache.is_some(), "message cache should remain valid for panel-only animation");
    }

    #[test]
    fn animation_tick_for_active_bash_result_invalidates_message_cache() {
        let mut app = test_app();
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "call_1".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app.push_msg(ChatMessage::ToolResult {
            tool_id: "call_1".to_string(),
            content: String::new(),
            elapsed_ms: None,
        });
        app.tool_start_time = Some(std::time::Instant::now());
        app.tool_start_times.insert("call_1".to_string(), std::time::Instant::now());
        app.line_cache = Some({
            let w = 80;
            let per_msg: Vec<Vec<ratatui::text::Line<'static>>> = (0..app.messages.len()).map(|i| app.render_message_lines(i, w)).collect();
            let flat: Vec<ratatui::text::Line<'static>> = per_msg.iter().flatten().cloned().collect();
            LineCache { width: w, per_msg, flat }
        });
        app.spinner_frame = 2;

        let invalidate_messages = app.advance_animations();

        assert!(invalidate_messages, "active message-area bash animation must rebuild message cache");
        assert!(!app.needs_clear_for_animation_redraw(), "streaming animation must not force whole-terminal clear flicker");
    }

    #[test]
    fn spinner_tick_with_thinking_placeholder_marks_only_tail_dirty() {
        // After advancing the spinner while THINKING_PLACEHOLDER is present,
        // only the last message slot should be marked dirty (dirty_from == last index).
        // Earlier per_msg slots must be untouched on the next rebuild.
        let mut app = test_app();
        let w = 80;
        app.push_msg(ChatMessage::User("question".to_string()));
        app.push_msg(ChatMessage::Text("partial response".to_string()));
        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));

        // Build full cache first
        app.line_cache = Some({
            let per_msg: Vec<Vec<ratatui::text::Line<'static>>> = (0..app.messages.len()).map(|i| app.render_message_lines(i, w)).collect();
            let flat: Vec<ratatui::text::Line<'static>> = per_msg.iter().flatten().cloned().collect();
            LineCache { width: w, per_msg, flat }
        });
        app.dirty_from = None;
        let last = app.messages.len() - 1;

        // Snapshot per_msg[0..last]
        let snapshot: Vec<Vec<String>> = app.line_cache.as_ref().unwrap().per_msg[..last]
            .iter()
            .map(|slot| slot.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect())
            .collect();

        // Advance spinner (spinner_frame % 3 == 0 triggers render_lines_uses_spinner)
        app.spinner_frame = 2; // next wrapping_add gives 3, which % 3 == 0
        let needs_redraw = app.advance_animations();
        assert!(needs_redraw, "spinner must signal redraw while THINKING_PLACEHOLDER present");

        // The animation tick itself doesn't call invalidate — the caller in mod.rs does.
        // We simulate that caller calling invalidate_last() (the new behaviour for slice 4).
        // But first assert dirty_from is still None (advance_animations doesn't set it).
        // Then we call invalidate_last() as the updated caller will.
        app.invalidate_last();
        assert_eq!(app.dirty_from, Some(last), "dirty_from must point to tail message only");

        // Simulate draw.rs incremental rebuild
        {
            let fresh: Vec<Vec<ratatui::text::Line<'static>>> = (last..app.messages.len())
                .map(|i| app.render_message_lines(i, w))
                .collect();
            let cache = app.line_cache.as_mut().unwrap();
            for (offset, rendered) in fresh.into_iter().enumerate() {
                cache.per_msg[last + offset] = rendered;
            }
            let prefix_len: usize = cache.per_msg[..last].iter().map(|v| v.len()).sum();
            cache.flat.truncate(prefix_len);
            for slot in &cache.per_msg[last..] {
                cache.flat.extend(slot.iter().cloned());
            }
        }
        app.dirty_from = None;

        // per_msg[0..last] must be unchanged
        let after: Vec<Vec<String>> = app.line_cache.as_ref().unwrap().per_msg[..last]
            .iter()
            .map(|slot| slot.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect())
            .collect();
        assert_eq!(snapshot, after, "earlier per_msg slots must not change on spinner tick");
    }

    #[test]
    fn grouped_system_output_does_not_insert_rules_between_indented_lines() {
        let mut app = test_app();
        app.push_msg(ChatMessage::System("Extensions (1):".to_string()));
        app.push_msg(ChatMessage::System("  capture — ok".to_string()));
        app.push_msg(ChatMessage::System("    tools: speak".to_string()));

        let lines = app.render_lines(80);
        let header_idx = lines
            .iter()
            .position(|line| line.spans.iter().any(|span| span.content.contains("Extensions (1):")))
            .expect("header system message should render");
        let child_idx = lines
            .iter()
            .position(|line| line.spans.iter().any(|span| span.content.contains("capture — ok")))
            .expect("child system message should render");
        let grandchild_idx = lines
            .iter()
            .position(|line| line.spans.iter().any(|span| span.content.contains("tools: speak")))
            .expect("grandchild system message should render");

        // Related system messages should not have extra blank-line separators between them
        // (they flow as a continuous block).
        let has_separator = |slice: &[ratatui::text::Line]| {
            // Two consecutive blank lines indicate a separator was inserted
            slice.windows(2).any(|w| {
                let blank = |l: &ratatui::text::Line| l.spans.is_empty() || l.spans.iter().all(|s| s.content.is_empty());
                blank(&w[0]) && blank(&w[1])
            })
        };
        assert!(!has_separator(&lines[header_idx + 1..child_idx]));
        assert!(!has_separator(&lines[child_idx + 1..grandchild_idx]));
    }

    #[test]
    fn unrelated_consecutive_system_messages_get_blank_line_separator() {
        let mut app = test_app();
        app.push_msg(ChatMessage::System("first".to_string()));
        app.push_msg(ChatMessage::System("second".to_string()));

        let lines = app.render_lines(80);
        let first_idx = lines
            .iter()
            .position(|line| line.spans.iter().any(|span| span.content.contains("first")))
            .expect("first system message should render");
        let second_idx = lines
            .iter()
            .position(|line| line.spans.iter().any(|span| span.content.contains("second")))
            .expect("second system message should render");

        let between = &lines[first_idx + 1..second_idx];
        let is_blank = |line: &ratatui::text::Line| {
            line.spans.is_empty() || line.spans.iter().all(|span| span.content.is_empty())
        };
        assert!(between.iter().any(is_blank), "expected blank line between consecutive system messages");
    }

    #[test]
    fn pasted_message_display_preserves_text_typed_after_paste() {
        let mut app = test_app();
        app.input_before_paste = Some("before".to_string());
        app.pasted_char_count = "PASTED".chars().count();

        let display = app.user_display_text_for_submission("beforePASTED after");

        assert_eq!(display, "before [Pasted 6 chars] after");
    }

    // ── Parallel tool-event routing (Bug 2 regression coverage) ─────────

    fn last_tool_use(app: &App, tool_id: &str) -> Option<(String, String)> {
        app.messages.iter().find_map(|m| match &m.msg {
            ChatMessage::ToolUse { tool_id: tid, tool_name, input } if tid == tool_id => {
                Some((tool_name.clone(), input.clone()))
            }
            _ => None,
        })
    }

    fn tool_use_start_partial(app: &App, tool_id: &str) -> Option<String> {
        app.messages.iter().find_map(|m| match &m.msg {
            ChatMessage::ToolUseStart { tool_id: tid, partial_input, .. } if tid == tool_id => {
                Some(partial_input.clone())
            }
            _ => None,
        })
    }

    fn tool_result_content(app: &App, tool_id: &str) -> Option<String> {
        app.messages.iter().find_map(|m| match &m.msg {
            ChatMessage::ToolResult { tool_id: tid, content, .. } if tid == tool_id => {
                Some(content.clone())
            }
            _ => None,
        })
    }

    #[test]
    fn parallel_tool_use_deltas_are_routed_by_tool_id() {
        // Regression: deltas from two interleaved parallel tool calls
        // must each land on their own ToolUseStart block. The pre-fix
        // behavior always appended to the most recent ToolUseStart,
        // corrupting the second tool's input with the first's deltas.
        let mut app = test_app();
        app.on_tool_use_start("call_a".to_string(), "bash".to_string());
        app.on_tool_use_start("call_b".to_string(), "read".to_string());

        // Codex sends deltas in interleaved order — exercise that.
        app.on_tool_use_delta("call_b", "{\"path\":");
        app.on_tool_use_delta("call_a", "{\"command\":");
        app.on_tool_use_delta("call_a", "\"ls\"}");
        app.on_tool_use_delta("call_b", "\"a\"}");

        assert_eq!(
            tool_use_start_partial(&app, "call_a").as_deref(),
            Some(r#"{"command":"ls"}"#),
            "call_a partial input must accumulate only call_a's deltas"
        );
        assert_eq!(
            tool_use_start_partial(&app, "call_b").as_deref(),
            Some(r#"{"path":"a"}"#),
            "call_b partial input must accumulate only call_b's deltas"
        );
    }

    #[test]
    fn parallel_tool_use_finalize_collapses_matching_start() {
        // Regression: finalize event must replace the matching
        // ToolUseStart in place, not push a new ToolUse at the end —
        // so on-screen order matches the order tools were called.
        let mut app = test_app();
        app.on_tool_use_start("call_a".to_string(), "bash".to_string());
        app.on_tool_use_start("call_b".to_string(), "read".to_string());

        app.on_tool_use_finalized(
            "call_a".to_string(),
            "bash".to_string(),
            r#"{"command":"ls"}"#.to_string(),
        );
        app.on_tool_use_finalized(
            "call_b".to_string(),
            "read".to_string(),
            r#"{"path":"a"}"#.to_string(),
        );

        // Both are now ToolUse, no lingering ToolUseStart.
        let lingering_starts = app
            .messages
            .iter()
            .filter(|m| matches!(&m.msg, ChatMessage::ToolUseStart { .. }))
            .count();
        assert_eq!(
            lingering_starts, 0,
            "every ToolUseStart must collapse on finalize — leftover starts cause perpetual bash-trace animations"
        );
        assert_eq!(
            last_tool_use(&app, "call_a"),
            Some(("bash".to_string(), r#"{"command":"ls"}"#.to_string()))
        );
        assert_eq!(
            last_tool_use(&app, "call_b"),
            Some(("read".to_string(), r#"{"path":"a"}"#.to_string()))
        );

        // On-screen order matches call order (call_a appears before call_b).
        let positions: Vec<&str> = app
            .messages
            .iter()
            .filter_map(|m| match &m.msg {
                ChatMessage::ToolUse { tool_id, .. } => Some(tool_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(positions, vec!["call_a", "call_b"]);
    }

    #[test]
    fn parallel_tool_results_do_not_overwrite_each_other() {
        // Regression: two ToolResult events arriving back-to-back must
        // each land on their own block by tool_id. The pre-fix path
        // pushed the first as a new message and then *overwrote* it
        // with the second — losing the first tool's output entirely.
        let mut app = test_app();
        app.on_tool_use_start("call_a".to_string(), "bash".to_string());
        app.on_tool_use_start("call_b".to_string(), "bash".to_string());
        app.on_tool_use_finalized(
            "call_a".to_string(),
            "bash".to_string(),
            "{}".to_string(),
        );
        app.on_tool_use_finalized(
            "call_b".to_string(),
            "bash".to_string(),
            "{}".to_string(),
        );

        app.on_tool_result("call_a".to_string(), "first output".to_string());
        app.on_tool_result("call_b".to_string(), "second output".to_string());

        assert_eq!(
            tool_result_content(&app, "call_a").as_deref(),
            Some("first output"),
            "call_a's result must survive — was overwritten by call_b in the buggy implementation"
        );
        assert_eq!(
            tool_result_content(&app, "call_b").as_deref(),
            Some("second output")
        );
    }

    #[test]
    fn tool_result_delta_streams_into_matching_block() {
        let mut app = test_app();
        app.on_tool_use_start("call_a".to_string(), "bash".to_string());
        app.on_tool_use_start("call_b".to_string(), "bash".to_string());
        app.on_tool_use_finalized(
            "call_a".to_string(),
            "bash".to_string(),
            "{}".to_string(),
        );
        app.on_tool_use_finalized(
            "call_b".to_string(),
            "bash".to_string(),
            "{}".to_string(),
        );

        // Interleaved deltas — must accumulate into the right block.
        app.on_tool_result_delta("call_a".to_string(), "alpha-".to_string());
        app.on_tool_result_delta("call_b".to_string(), "beta-".to_string());
        app.on_tool_result_delta("call_a".to_string(), "one".to_string());
        app.on_tool_result_delta("call_b".to_string(), "two".to_string());

        // Then finalize results.
        app.on_tool_result("call_a".to_string(), "alpha-one".to_string());
        app.on_tool_result("call_b".to_string(), "beta-two".to_string());

        assert_eq!(
            tool_result_content(&app, "call_a").as_deref(),
            Some("alpha-one")
        );
        assert_eq!(
            tool_result_content(&app, "call_b").as_deref(),
            Some("beta-two")
        );
    }

    #[test]
    fn parallel_tool_results_record_per_tool_elapsed_time() {
        let mut app = test_app();
        app.on_tool_use_start("call_a".to_string(), "bash".to_string());
        app.on_tool_use_start("call_b".to_string(), "bash".to_string());

        // Sleep deliberately tiny so Instant::elapsed > 0 is guaranteed.
        std::thread::sleep(std::time::Duration::from_millis(2));
        app.on_tool_result("call_a".to_string(), "a".to_string());
        std::thread::sleep(std::time::Duration::from_millis(2));
        app.on_tool_result("call_b".to_string(), "b".to_string());

        let a_elapsed = app.messages.iter().find_map(|m| match &m.msg {
            ChatMessage::ToolResult { tool_id, elapsed_ms, .. } if tool_id == "call_a" => *elapsed_ms,
            _ => None,
        });
        let b_elapsed = app.messages.iter().find_map(|m| match &m.msg {
            ChatMessage::ToolResult { tool_id, elapsed_ms, .. } if tool_id == "call_b" => *elapsed_ms,
            _ => None,
        });
        assert!(a_elapsed.is_some(), "call_a must record elapsed_ms from its own start_time");
        assert!(b_elapsed.is_some(), "call_b must record elapsed_ms from its own start_time");
    }

    // ── FIX 9 regression tests ───────────────────────────────────────────────

    /// Test A: a REAL "…" (plain ellipsis) in model output must survive — only
    /// the sentinel THINKING_PLACEHOLDER (ellipsis + ZWSP) triggers the drop.
    #[test]
    fn real_ellipsis_thinking_content_is_not_dropped() {
        let mut app = test_app();
        // Push the sentinel placeholder (what the stream start inserts)
        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
        // First delta is a plain "…" (model literally output ellipsis)
        app.append_or_update_thinking("…");
        // More text follows
        app.append_or_update_text("answer");
        // The Thinking block must survive with "…" content, not be dropped
        assert!(
            app.messages.iter().any(|m| matches!(&m.msg, ChatMessage::Thinking(t) if t == "…")),
            "real ellipsis thinking content must survive — sentinel and real output are distinct"
        );
    }

    /// Test B: THINKING_PLACEHOLDER stranded by a System/Notice message on top
    /// must be cleaned up by drop_empty_thinking.
    #[test]
    fn placeholder_stranded_by_system_message_is_dropped() {
        let mut app = test_app();
        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
        // A Notice arrives on top (as happens in stream_handler::SessionEvent::Notice)
        app.push_msg(ChatMessage::System("retrying…".to_string()));
        // Now trigger the drop (e.g. text arrives / turn ends)
        app.drop_empty_thinking();
        let has_placeholder = app.messages.iter().any(|m| matches!(
            &m.msg, ChatMessage::Thinking(t) if t == THINKING_PLACEHOLDER
        ));
        assert!(!has_placeholder, "placeholder stranded under a System message must be removed by drop_empty_thinking");
    }

    /// Test C: aborting a stream must not leave a frozen placeholder spinner.
    #[test]
    fn abort_path_clears_thinking_placeholder() {
        let mut app = test_app();
        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
        // Simulate what the abort handler does: drop + push error
        app.drop_empty_thinking();
        app.push_msg(ChatMessage::Error("aborted".to_string()));
        let has_placeholder = app.messages.iter().any(|m| matches!(
            &m.msg, ChatMessage::Thinking(t) if t == THINKING_PLACEHOLDER
        ));
        assert!(!has_placeholder, "abort must remove thinking placeholder so spinner doesn't freeze");
    }

    #[test]
    fn render_lines_equals_concat_of_render_message_lines() {
        let mut app = test_app();
        app.push_msg(ChatMessage::User("hello world".to_string()));
        app.push_msg(ChatMessage::Thinking("some reasoning".to_string()));
        app.push_msg(ChatMessage::Text("here is the answer".to_string()));
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "call_1".to_string(),
            tool_name: "bash".to_string(),
            input: r#"{"command":"ls"}"#.to_string(),
        });
        app.push_msg(ChatMessage::ToolResult {
            tool_id: "call_1".to_string(),
            content: "file1.txt\nfile2.txt".to_string(),
            elapsed_ms: Some(42),
        });

        let w = 80;
        let flat = app.render_lines(w);
        let concat: Vec<ratatui::text::Line<'static>> = (0..app.messages.len())
            .flat_map(|i| app.render_message_lines(i, w))
            .collect();

        // Compare by rendered string content (Line doesn't impl PartialEq for spans reliably)
        let to_str = |lines: &[ratatui::text::Line<'static>]| -> Vec<String> {
            lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect()
        };
        assert_eq!(to_str(&flat), to_str(&concat),
            "render_lines must equal concat of render_message_lines for each index");
    }

    #[test]
    fn line_cache_build_produces_flat_equal_to_render_lines() {
        // This test verifies that the LineCache struct's build path (per_msg concat -> flat)
        // produces the same flat output as render_lines(width).
        let mut app = test_app();
        app.push_msg(ChatMessage::User("hi".to_string()));
        app.push_msg(ChatMessage::Text("hello back".to_string()));
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "t1".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app.push_msg(ChatMessage::ToolResult {
            tool_id: "t1".to_string(),
            content: "output".to_string(),
            elapsed_ms: Some(10),
        });

        let w = 80;
        let expected_flat = app.render_lines(w);

        // Build a LineCache manually using the new struct
        let per_msg: Vec<Vec<ratatui::text::Line<'static>>> = (0..app.messages.len())
            .map(|i| app.render_message_lines(i, w))
            .collect();
        let flat: Vec<ratatui::text::Line<'static>> = per_msg.iter().flatten().cloned().collect();
        let cache = LineCache { width: w, per_msg, flat };

        let to_str = |lines: &[ratatui::text::Line<'static>]| -> Vec<String> {
            lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect()
        };
        assert_eq!(to_str(&expected_flat), to_str(&cache.flat),
            "LineCache.flat must equal render_lines output");
        assert!(cache.width == w);
    }

    /// Helper: builds a LineCache for an app at a given width, simulating what draw.rs does.
    fn build_cache(app: &App, width: usize) -> LineCache {
        let per_msg: Vec<Vec<ratatui::text::Line<'static>>> = (0..app.messages.len())
            .map(|i| app.render_message_lines(i, width))
            .collect();
        let flat: Vec<ratatui::text::Line<'static>> = per_msg.iter().flatten().cloned().collect();
        LineCache { width, per_msg, flat }
    }

    /// Helper: simulate the incremental rebuild from draw.rs (slice 3 logic).
    fn rebuild_incremental(app: &mut App, width: usize) {
        match app.line_cache.take() {
            Some(mut cache) if cache.width == width => {
                if let Some(k) = app.dirty_from.take() {
                    // If per_msg length is out of sync with messages (insert/remove), re-render all from k.
                    if cache.per_msg.len() != app.messages.len() {
                        cache.per_msg.truncate(k);
                        for i in k..app.messages.len() {
                            cache.per_msg.push(app.render_message_lines(i, width));
                        }
                    } else {
                        // In-place partial re-render: only messages[k..]
                        for i in k..app.messages.len() {
                            cache.per_msg[i] = app.render_message_lines(i, width);
                        }
                    }
                    // Rebuild flat from k: keep lines from per_msg[..k], append from per_msg[k..]
                    let prefix_len: usize = cache.per_msg[..k].iter().map(|v| v.len()).sum();
                    cache.flat.truncate(prefix_len);
                    for slot in &cache.per_msg[k..] {
                        cache.flat.extend(slot.iter().cloned());
                    }
                }
                app.line_cache = Some(cache);
            }
            _ => {
                app.dirty_from = None;
                app.line_cache = Some(build_cache(app, width));
            }
        }
    }

    #[test]
    fn incremental_cache_does_not_re_render_unchanged_messages() {
        let mut app = test_app();
        let w = 80;
        // Build fixture: User + Thinking + Text (streaming last)
        app.push_msg(ChatMessage::User("hello".to_string()));
        app.push_msg(ChatMessage::Thinking("reasoning".to_string()));
        app.push_msg(ChatMessage::Text("partial answer".to_string()));

        // Full build
        app.line_cache = Some(build_cache(&app, w));
        app.dirty_from = None;

        let last = app.messages.len() - 1; // index of Text message

        // Snapshot per_msg[0..last] content strings (before update)
        let snapshot: Vec<Vec<String>> = app.line_cache.as_ref().unwrap().per_msg[..last]
            .iter()
            .map(|slot| slot.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect())
            .collect();

        // Simulate append_or_update_text delta (modifies last message, marks tail dirty)
        if let Some(crate::tui::app::TimestampedMsg { msg: ChatMessage::Text(ref mut t), .. }) = app.messages.last_mut() {
            t.push_str(" — more content appended");
        }
        app.dirty_from = Some(last); // invalidate_last equivalent

        // Incremental rebuild
        rebuild_incremental(&mut app, w);

        let cache = app.line_cache.as_ref().unwrap();

        // per_msg[0..last] must be unchanged (content-equal to snapshot)
        let after: Vec<Vec<String>> = cache.per_msg[..last]
            .iter()
            .map(|slot| slot.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect())
            .collect();
        assert_eq!(snapshot, after, "per_msg[0..last] must not change on tail-only invalidation");

        // per_msg[last] must have changed (the text grew)
        let last_strs: Vec<String> = cache.per_msg[last].iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();
        let contains_new = last_strs.iter().any(|s| s.contains("more content appended"));
        assert!(contains_new, "per_msg[last] must reflect the updated text");

        // flat must equal full render_lines (correctness)
        let expected_flat = app.render_lines(w);
        let to_str = |lines: &[ratatui::text::Line<'static>]| -> Vec<String> {
            lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect()
        };
        assert_eq!(to_str(&expected_flat), to_str(&cache.flat),
            "flat must equal full render_lines after incremental rebuild");
    }

    #[test]
    fn incremental_cache_handles_tool_result_insert() {
        let mut app = test_app();
        let w = 80;
        app.push_msg(ChatMessage::User("run something".to_string()));
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "t1".to_string(),
            tool_name: "bash".to_string(),
            input: r#"{"command":"ls"}"#.to_string(),
        });

        app.line_cache = Some(build_cache(&app, w));
        app.dirty_from = None;

        // Insert a ToolResult after ToolUse (as push_tool_result does)
        let at = 2;
        app.messages.insert(at, crate::tui::app::TimestampedMsg {
            msg: ChatMessage::ToolResult {
                tool_id: "t1".to_string(),
                content: "file.txt".to_string(),
                elapsed_ms: Some(5),
            },
            time: "00:00".to_string(),
        });
        app.dirty_from = Some(at); // invalidate_from(at)

        // Rebuild: since per_msg.len() (2) != messages.len() (3), should do full-from-k rebuild
        rebuild_incremental(&mut app, w);

        let cache = app.line_cache.as_ref().unwrap();
        assert_eq!(cache.per_msg.len(), app.messages.len(), "per_msg must track messages after insert");

        let expected_flat = app.render_lines(w);
        let to_str = |lines: &[ratatui::text::Line<'static>]| -> Vec<String> {
            lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect()
        };
        assert_eq!(to_str(&expected_flat), to_str(&cache.flat),
            "flat must equal render_lines after insert + incremental rebuild");
    }
}
