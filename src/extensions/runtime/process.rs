//! Process-based extension runtime — JSON-RPC 2.0 over stdio.
//!
//! Spawns the extension as a child process. Communication uses
//! Content-Length framing (LSP-style) over stdin/stdout.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio::task::JoinHandle;

use super::{ExtensionHandler, ExtensionHealth, RestartPolicy};
use crate::extensions::hooks::events::{HookEvent, HookResult};
use crate::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION;

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    method: String,
    params: Value,
    id: u64,
}


#[derive(Serialize)]
struct InitializeParams {
    synaps_version: &'static str,
    extension_protocol_version: u32,
    plugin_id: String,
    plugin_root: Option<String>,
    config: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RegisteredExtensionToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RegisteredProviderSpec {
    pub id: String,
    pub display_name: String,
    pub description: String,
    #[serde(default)]
    pub models: Vec<RegisteredProviderModelSpec>,
    #[serde(default)]
    pub config_schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RegisteredProviderModelSpec {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub capabilities: Value,
    #[serde(default)]
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCompleteParams {
    pub provider_id: String,
    pub model_id: String,
    pub model: String,
    pub messages: Vec<Value>,
    pub system_prompt: Option<String>,
    pub tools: Vec<Value>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub thinking_budget: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProviderCompleteResult {
    pub content: Vec<Value>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// A single streaming event from a provider extension.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderStreamEvent {
    /// Incremental assistant text.
    TextDelta { text: String },
    /// Incremental thinking text.
    ThinkingDelta { text: String },
    /// A complete tool-use block (matches ProviderToolUse fields).
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// Usage metadata (typically near end-of-stream).
    Usage { usage: Value },
    /// Provider-side error (non-fatal stream notification — caller decides).
    Error { message: String },
    /// Optional explicit end-of-stream marker.
    Done,
}

/// Parse a single `provider.stream.event` notification's `params` value into a
/// [`ProviderStreamEvent`]. Returns `Err(String)` on malformed input.
///
/// Accepts both `{"event": {"type": "...", ...}}` and flat `{"type": "...", ...}`
/// shapes.
pub fn parse_provider_stream_event(params: &Value) -> Result<ProviderStreamEvent, String> {
    let inner = match params.get("event") {
        Some(ev) => ev,
        None => params,
    };
    let obj = inner
        .as_object()
        .ok_or_else(|| "provider stream event must be a JSON object".to_string())?;

    let ty = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "provider stream event missing type".to_string())?;

    match ty {
        "text" => {
            let text = obj
                .get("delta")
                .or_else(|| obj.get("text"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    "provider stream text event missing 'delta' or 'text'".to_string()
                })?;
            Ok(ProviderStreamEvent::TextDelta {
                text: text.to_string(),
            })
        }
        "thinking" => {
            let text = obj
                .get("delta")
                .or_else(|| obj.get("text"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    "provider stream thinking event missing 'delta' or 'text'".to_string()
                })?;
            Ok(ProviderStreamEvent::ThinkingDelta {
                text: text.to_string(),
            })
        }
        "tool_use" => {
            let id = obj
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "provider stream tool_use missing id".to_string())?;
            if id.is_empty() {
                return Err("provider stream tool_use id must be non-empty".to_string());
            }
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "provider stream tool_use missing name".to_string())?;
            if name.is_empty() {
                return Err("provider stream tool_use name must be non-empty".to_string());
            }
            let input = match obj.get("input") {
                None => Value::Object(Default::default()),
                Some(v) if v.is_object() => v.clone(),
                Some(_) => {
                    return Err(
                        "provider stream tool_use input must be a JSON object".to_string()
                    );
                }
            };
            Ok(ProviderStreamEvent::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
            })
        }
        "usage" => {
            let mut clone = obj.clone();
            clone.remove("type");
            Ok(ProviderStreamEvent::Usage {
                usage: Value::Object(clone),
            })
        }
        "error" => {
            let message = obj
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| "provider stream error missing message".to_string())?;
            if message.is_empty() {
                return Err("provider stream error message must be non-empty".to_string());
            }
            Ok(ProviderStreamEvent::Error {
                message: message.to_string(),
            })
        }
        "done" => Ok(ProviderStreamEvent::Done),
        other => Err(format!("unknown provider stream event type: {other}")),
    }
}

pub async fn execute_provider_tool_use(
    registry: &crate::ToolRegistry,
    hook_bus: &Arc<crate::extensions::hooks::HookBus>,
    tool_use: ProviderToolUse,
    ctx: crate::ToolContext,
    max_tool_output: usize,
) -> Value {
    let tool_id = tool_use.id;
    let tool_name = tool_use.name;
    let input = tool_use.input;

    let Some(tool) = registry.get(&tool_name).cloned() else {
        return serde_json::json!({
            "type": "tool_result",
            "tool_use_id": tool_id,
            "content": format!("Unknown tool: {}", tool_name),
            "is_error": true,
        });
    };

    let runtime_name = registry.runtime_name_for_api(&tool_name).to_string();
    let input = registry.translate_input_for_api_tool(&tool_name, input);
    let decision = crate::runtime::resolve_before_tool_call_decision(
        input.clone(),
        crate::runtime::emit_before_tool_call(
            hook_bus,
            &tool_name,
            Some(&runtime_name),
            input.clone(),
        ).await,
        ctx.capabilities.secret_prompt.as_ref(),
    ).await;

    let crate::runtime::BeforeToolCallDecision::Continue { input } = decision else {
        let crate::runtime::BeforeToolCallDecision::Block { reason } = decision else { unreachable!() };
        return serde_json::json!({
            "type": "tool_result",
            "tool_use_id": tool_id,
            "content": format!("Tool call blocked by extension: {}", reason),
            "is_error": true,
        });
    };

    let input_for_hook = input.clone();
    let (result, is_error) = match tool.execute(input, ctx).await {
        Ok(output) => (output, false),
        Err(error) => (format!("Tool execution failed: {}", error), true),
    };
    let _ = crate::runtime::emit_after_tool_call(
        hook_bus,
        &tool_name,
        Some(&runtime_name),
        input_for_hook,
        result.clone(),
    ).await;

    let mut response = serde_json::json!({
        "type": "tool_result",
        "tool_use_id": tool_id,
        "content": crate::truncate_str(&result, max_tool_output).to_string(),
    });
    if is_error {
        response["is_error"] = serde_json::json!(true);
    }
    response
}

pub async fn complete_provider_with_tools<F>(
    handler: Arc<dyn ExtensionHandler>,
    mut params: ProviderCompleteParams,
    registry: &crate::ToolRegistry,
    hook_bus: &Arc<crate::extensions::hooks::HookBus>,
    mut context_factory: F,
    max_tool_output: usize,
    max_iterations: usize,
) -> Result<ProviderCompleteResult, String>
where
    F: FnMut() -> crate::ToolContext,
{
    let max_iterations = max_iterations.max(1);
    for iteration in 0..max_iterations {
        let result = handler.provider_complete(params.clone()).await?;
        let tool_uses = extract_provider_tool_uses(&result.content)?;
        if tool_uses.is_empty() {
            return Ok(result);
        }
        if iteration + 1 == max_iterations {
            return Err(format!(
                "extension provider '{}' exceeded provider tool-use iteration limit ({})",
                handler.id(),
                max_iterations,
            ));
        }

        let assistant_content = result.content.clone();
        params.messages.push(serde_json::json!({
            "role": "assistant",
            "content": assistant_content,
        }));

        let mut tool_results = Vec::with_capacity(tool_uses.len());
        for tool_use in tool_uses {
            tool_results.push(execute_provider_tool_use(
                registry,
                hook_bus,
                tool_use,
                context_factory(),
                max_tool_output,
            ).await);
        }
        params.messages.push(serde_json::json!({
            "role": "user",
            "content": tool_results,
        }));
    }
    Err(format!(
        "extension provider '{}' exceeded provider tool-use iteration limit ({})",
        handler.id(),
        max_iterations,
    ))
}

pub fn extract_provider_tool_uses(content: &[Value]) -> Result<Vec<ProviderToolUse>, String> {
    let mut tool_uses = Vec::new();
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let id = block
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "provider tool_use missing id".to_string())?;
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "provider tool_use missing name".to_string())?;
        if id.trim().is_empty() {
            return Err("provider tool_use id is empty".to_string());
        }
        if name.trim().is_empty() {
            return Err("provider tool_use name is empty".to_string());
        }
        let input = block
            .get("input")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        if !input.is_object() {
            return Err(format!(
                "provider tool_use '{}' input must be a JSON object",
                id
            ));
        }
        tool_uses.push(ProviderToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
        });
    }
    Ok(tool_uses)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InitializeCapabilitiesResult {
    pub tools: Vec<RegisteredExtensionToolSpec>,
    pub providers: Vec<RegisteredProviderSpec>,
    pub voice: Option<VoiceCapabilityDeclaration>,
}

/// Declaration of a voice capability provided by an extension.
///
/// The actual sidecar implementation lives in the plugin (see
/// `synaps-skills/`); core only tracks the metadata so it can surface
/// the capability in `/extensions status` and gate the corresponding
/// audio permissions.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct VoiceCapabilityDeclaration {
    /// Display name, e.g. "Local Whisper STT".
    pub name: String,
    /// Modes supported: subset of ["stt", "tts", "wake_word"].
    pub modes: Vec<String>,
    /// Optional sidecar endpoint (e.g. "http://127.0.0.1:8723"). Informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// Validate a [`VoiceCapabilityDeclaration`] against the granted permission set.
///
/// Rules:
/// - `name` must be non-empty.
/// - `modes` must be non-empty and only contain `stt`, `tts`, or `wake_word`.
/// - `stt` or `wake_word` modes require the `audio.input` permission.
/// - `tts` mode requires the `audio.output` permission.
pub fn validate_voice_capability(
    decl: &VoiceCapabilityDeclaration,
    permissions: &crate::extensions::permissions::PermissionSet,
) -> Result<(), String> {
    use crate::extensions::permissions::Permission;
    if decl.name.trim().is_empty() {
        return Err("voice capability 'name' must be non-empty".to_string());
    }
    if decl.modes.is_empty() {
        return Err("voice capability 'modes' must be non-empty".to_string());
    }
    for mode in &decl.modes {
        match mode.as_str() {
            "stt" | "wake_word" => {
                if !permissions.has(Permission::AudioInput) {
                    return Err(format!(
                        "voice capability mode '{}' requires permission 'audio.input'",
                        mode
                    ));
                }
            }
            "tts" => {
                if !permissions.has(Permission::AudioOutput) {
                    return Err(
                        "voice capability mode 'tts' requires permission 'audio.output'"
                            .to_string(),
                    );
                }
            }
            other => {
                return Err(format!(
                    "voice capability declares unknown mode '{}' (expected one of 'stt', 'tts', 'wake_word')",
                    other
                ));
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct InitializeResult {
    protocol_version: u32,
    #[serde(default)]
    capabilities: InitializeCapabilities,
}

#[derive(Default, Deserialize)]
struct InitializeCapabilities {
    #[serde(default)]
    tools: Vec<RegisteredExtensionToolSpec>,
    #[serde(default)]
    providers: Vec<RegisteredProviderSpec>,
    #[serde(default)]
    voice: Option<VoiceCapabilityDeclaration>,
}

/// A JSON-RPC notification frame received from an extension (no `id`).
///
/// Internal API exposed publicly (`#[doc(hidden)]`) so integration tests
/// can subscribe to notifications via [`ProcessExtension::subscribe_notifications`].
/// Extension authors should not depend on this type — it may change without notice.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct NotificationFrame {
    pub method: String,
    pub params: Value,
}

/// Shared mailbox for the background reader task. Holds in-flight request
/// senders (keyed by JSON-RPC id) and an optional notification subscriber.
///
/// Persists across process restarts: a new reader task replaces the old one
/// but the `Inbox` itself is reused. Pending requests are drained with
/// errors when the reader observes EOF or a transport failure.
struct Inbox {
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    notification_sink: Mutex<Option<mpsc::UnboundedSender<NotificationFrame>>>,
    /// Permissions granted to the calling extension. Set after manifest
    /// validation; checked by inbound RPC handlers (e.g. memory.append).
    permissions: RwLock<Option<crate::extensions::permissions::PermissionSet>>,
    /// Stdin handle of the currently-running child process. Used by the
    /// reader task to write JSON-RPC responses for inbound requests.
    /// Replaced on each spawn (initial spawn + restarts).
    inbound_stdin: Mutex<Option<Arc<Mutex<ChildStdin>>>>,
    /// Extension id, used for namespace policy and diagnostics.
    extension_id: String,
}

impl Inbox {
    fn new(extension_id: String) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            notification_sink: Mutex::new(None),
            permissions: RwLock::new(None),
            inbound_stdin: Mutex::new(None),
            extension_id,
        }
    }

    /// Drains all pending request senders, sending `Err(reason)` to each.
    async fn fail_all_pending(&self, reason: &str) {
        let drained: Vec<_> = {
            let mut pending = self.pending.lock().await;
            pending.drain().collect()
        };
        for (_, tx) in drained {
            let _ = tx.send(Err(reason.to_string()));
        }
    }
}

struct ProcessState {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    reader_handle: JoinHandle<()>,
}

/// A running extension process communicating via JSON-RPC 2.0 over stdio.
pub struct ProcessExtension {
    id: String,
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    state: Arc<Mutex<Option<ProcessState>>>,
    /// Serializes a full request/response exchange and restart attempts.
    call_lock: Arc<Mutex<()>>,
    next_id: AtomicU64,
    restart_count: AtomicUsize,
    /// Restart policy controlling exponential backoff and budget.
    pub(crate) restart_policy: RestartPolicy,
    /// Shared mailbox between the reader task and request callers. Persists
    /// across process restarts so that any active notification subscriber
    /// survives a restart-on-error.
    inbox: Arc<Inbox>,
}

impl ProcessExtension {
    pub async fn spawn(id: &str, command: &str, args: &[String]) -> Result<Self, String> {
        Self::spawn_with_cwd(id, command, args, None).await
    }

    /// Spawn `command` with `args` and optional working directory.
    ///
    /// Child stderr is captured and forwarded to debug tracing with the extension
    /// id so extension authors can inspect diagnostics without corrupting stdout.
    pub async fn spawn_with_cwd(
        id: &str,
        command: &str,
        args: &[String],
        cwd: Option<PathBuf>,
    ) -> Result<Self, String> {
        let inbox = Arc::new(Inbox::new(id.to_string()));
        let state = Self::spawn_state(id, command, args, cwd.as_ref(), inbox.clone()).await?;
        Ok(Self {
            id: id.to_string(),
            command: command.to_string(),
            args: args.to_vec(),
            cwd,
            state: Arc::new(Mutex::new(Some(state))),
            call_lock: Arc::new(Mutex::new(())),
            next_id: AtomicU64::new(1),
            restart_count: AtomicUsize::new(0),
            restart_policy: RestartPolicy::default(),
            inbox,
        })
    }

    /// Override the restart policy. Intended for tests.
    pub fn with_restart_policy(mut self, policy: RestartPolicy) -> Self {
        self.restart_policy = policy;
        self
    }

    async fn spawn_state(
        id: &str,
        command: &str,
        args: &[String],
        cwd: Option<&PathBuf>,
        inbox: Arc<Inbox>,
    ) -> Result<ProcessState, String> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn extension '{}': {}", id, e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("No stdin for extension '{}'", id))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("No stdout for extension '{}'", id))?;
        if let Some(stderr) = child.stderr.take() {
            let extension_id = id.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            tracing::debug!(extension = %extension_id, stderr = %line);
                        }
                        Ok(None) => break,
                        Err(error) => {
                            tracing::debug!(
                                extension = %extension_id,
                                error = %error,
                                "Failed to read extension stderr",
                            );
                            break;
                        }
                    }
                }
            });
        }

        let reader_handle = Self::spawn_reader(stdout, inbox.clone(), id.to_string());

        let stdin_arc = Arc::new(Mutex::new(stdin));
        // Publish current stdin into the inbox so the reader task can write
        // JSON-RPC responses for inbound requests (e.g. memory.append).
        *inbox.inbound_stdin.lock().await = Some(stdin_arc.clone());

        Ok(ProcessState {
            child,
            stdin: stdin_arc,
            reader_handle,
        })
    }

    /// Spawn the background reader task that owns `stdout`, demultiplexing
    /// JSON-RPC responses (by id) and notifications (no id) into the shared
    /// [`Inbox`]. Returns a `JoinHandle` so callers can `.abort()` it on
    /// restart or shutdown.
    fn spawn_reader(
        stdout: ChildStdout,
        inbox: Arc<Inbox>,
        extension_id: String,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match Self::read_one_frame(&mut reader, &extension_id).await {
                    Ok(Some(value)) => {
                        Self::dispatch_frame(value, &inbox, &extension_id).await;
                    }
                    Ok(None) => {
                        tracing::debug!(
                            extension = %extension_id,
                            "Extension stdout closed (EOF); failing pending requests",
                        );
                        inbox.fail_all_pending("transport closed: EOF").await;
                        // Drop notification subscriber on EOF.
                        inbox.notification_sink.lock().await.take();
                        return;
                    }
                    Err(error) => {
                        tracing::debug!(
                            extension = %extension_id,
                            error = %error,
                            "Extension transport read error",
                        );
                        inbox
                            .fail_all_pending(&format!("transport error: {}", error))
                            .await;
                        inbox.notification_sink.lock().await.take();
                        return;
                    }
                }
            }
        })
    }

    /// Read one Content-Length-framed JSON message from `reader`. Returns
    /// `Ok(None)` on a clean EOF *before* any header bytes are read; any
    /// other unexpected EOF is reported as `Err`.
    async fn read_one_frame(
        reader: &mut BufReader<ChildStdout>,
        extension_id: &str,
    ) -> Result<Option<Value>, String> {
        let mut content_length: Option<usize> = None;
        let mut saw_any_header = false;
        loop {
            let mut header_line = String::new();
            let n = reader
                .read_line(&mut header_line)
                .await
                .map_err(|e| format!("Read header error: {}", e))?;
            if n == 0 {
                if saw_any_header {
                    return Err("Unexpected EOF while reading response headers".into());
                }
                return Ok(None);
            }
            saw_any_header = true;
            if header_line.len() > 1024 {
                return Err(format!(
                    "Extension '{}' header line too long ({} bytes)",
                    extension_id,
                    header_line.len()
                ));
            }
            let trimmed = header_line.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                if name.trim().eq_ignore_ascii_case("Content-Length") {
                    content_length = Some(value.trim().parse().map_err(|_| {
                        format!("Invalid Content-Length value: {:?}", value.trim())
                    })?);
                }
            }
        }
        let content_length = content_length.ok_or_else(|| {
            format!(
                "Extension '{}' frame missing Content-Length header",
                extension_id
            )
        })?;
        const MAX_RESPONSE_SIZE: usize = 4 * 1024 * 1024;
        if content_length > MAX_RESPONSE_SIZE {
            return Err(format!(
                "Extension '{}' frame too large: {} bytes (max {})",
                extension_id, content_length, MAX_RESPONSE_SIZE
            ));
        }
        let mut buf = vec![0u8; content_length];
        tokio::io::AsyncReadExt::read_exact(reader, &mut buf)
            .await
            .map_err(|e| format!("Read body error: {}", e))?;
        let value: Value = serde_json::from_slice(&buf)
            .map_err(|e| format!("Parse frame error: {}", e))?;
        Ok(Some(value))
    }

    /// Route a parsed JSON-RPC frame to the right consumer:
    /// - response (`id` numeric, no `method`) → matching pending oneshot
    /// - request (`id` numeric and `method`) → inbound request handler
    /// - notification (`method` set, no `id`) → notification subscriber
    /// - anything else → trace and drop
    async fn dispatch_frame(value: Value, inbox: &Arc<Inbox>, extension_id: &str) {
        let id_field = value.get("id");
        let id_is_present = !matches!(id_field, None | Some(Value::Null));
        let method_field = value.get("method").and_then(Value::as_str).map(str::to_string);

        if id_is_present && method_field.is_some() {
            // Inbound request from the extension. Spawn a task to handle it
            // so the reader loop is never blocked on memory I/O or other work.
            let id = match id_field.and_then(Value::as_u64) {
                Some(id) => id,
                None => {
                    tracing::trace!(
                        extension = %extension_id,
                        frame = %value,
                        "Discarding inbound request with non-numeric id",
                    );
                    return;
                }
            };
            let method = method_field.unwrap();
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let inbox = inbox.clone();
            let extension_id = extension_id.to_string();
            tokio::spawn(async move {
                let outcome = Self::handle_inbound_request(&inbox, &method, params).await;
                let payload = match outcome {
                    Ok(result) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result,
                    }),
                    Err((code, message)) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": code, "message": message},
                    }),
                };
                let stdin_handle = inbox.inbound_stdin.lock().await.clone();
                if let Some(stdin) = stdin_handle {
                    let body = match serde_json::to_string(&payload) {
                        Ok(s) => s,
                        Err(error) => {
                            tracing::warn!(
                                extension = %extension_id,
                                error = %error,
                                "Failed to serialize inbound response",
                            );
                            return;
                        }
                    };
                    let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
                    let mut stdin = stdin.lock().await;
                    if let Err(error) = stdin.write_all(frame.as_bytes()).await {
                        tracing::warn!(
                            extension = %extension_id,
                            error = %error,
                            "Failed to write inbound response",
                        );
                        return;
                    }
                    if let Err(error) = stdin.flush().await {
                        tracing::warn!(
                            extension = %extension_id,
                            error = %error,
                            "Failed to flush inbound response",
                        );
                    }
                } else {
                    tracing::warn!(
                        extension = %extension_id,
                        "No stdin available to reply to inbound request",
                    );
                }
            });
            return;
        }

        if id_is_present {
            let id = match id_field.and_then(Value::as_u64) {
                Some(id) => id,
                None => {
                    tracing::trace!(
                        extension = %extension_id,
                        frame = %value,
                        "Discarding frame with non-numeric id",
                    );
                    return;
                }
            };
            let sender = inbox.pending.lock().await.remove(&id);
            match sender {
                Some(tx) => {
                    let payload = if let Some(err) = value.get("error") {
                        let message = err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown extension error")
                            .to_string();
                        Err(format!("Extension error: {}", message))
                    } else {
                        Ok(value
                            .get("result")
                            .cloned()
                            .unwrap_or(Value::Null))
                    };
                    let _ = tx.send(payload);
                }
                None => {
                    tracing::trace!(
                        extension = %extension_id,
                        id = id,
                        "Response with unknown id (no pending request); dropping",
                    );
                }
            }
        } else if let Some(method) = value.get("method").and_then(Value::as_str) {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let frame = NotificationFrame {
                method: method.to_string(),
                params,
            };
            let mut sink_guard = inbox.notification_sink.lock().await;
            if let Some(sink) = sink_guard.as_ref() {
                if sink.send(frame).is_err() {
                    // Receiver dropped; clear subscription.
                    sink_guard.take();
                }
            } else {
                tracing::trace!(
                    extension = %extension_id,
                    method = %method,
                    "Notification with no active subscriber; dropping",
                );
            }
        } else {
            tracing::trace!(
                extension = %extension_id,
                frame = %value,
                "Unrecognized frame; dropping",
            );
        }
    }

    pub fn restart_count(&self) -> usize {
        self.restart_count.load(Ordering::Relaxed)
    }

    /// Public for tests: set the permission set used by inbound RPC handlers
    /// (e.g. memory.append). Called by the manager after manifest validation.
    pub async fn set_permissions(&self, perms: crate::extensions::permissions::PermissionSet) {
        *self.inbox.permissions.write().await = Some(perms);
    }

    /// Handle a JSON-RPC request initiated by the extension.
    ///
    /// Returns `Ok(result_value)` on success or `Err((code, message))` for a
    /// JSON-RPC error response. Currently routes:
    /// - `memory.append` (requires `memory.write`)
    /// - `memory.query`  (requires `memory.read`)
    /// All other methods return -32601 (method not found).
    async fn handle_inbound_request(
        inbox: &Arc<Inbox>,
        method: &str,
        params: Value,
    ) -> Result<Value, (i32, String)> {
        use crate::extensions::permissions::Permission;
        use crate::memory::store::{self, MemoryQuery};

        match method {
            "memory.append" => {
                Self::require_permission(inbox, Permission::MemoryWrite, "memory.write").await?;
                let namespace = Self::param_str(&params, "namespace")?;
                Self::require_namespace_matches(inbox, &namespace).await?;
                let content = Self::param_str(&params, "content")?;
                let tags = match params.get("tags") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(arr)) => {
                        let mut out = Vec::with_capacity(arr.len());
                        for v in arr {
                            match v.as_str() {
                                Some(s) => out.push(s.to_string()),
                                None => {
                                    return Err((
                                        -32602,
                                        "tags must be an array of strings".to_string(),
                                    ))
                                }
                            }
                        }
                        out
                    }
                    _ => {
                        return Err((
                            -32602,
                            "tags must be an array of strings".to_string(),
                        ))
                    }
                };
                let meta = match params.get("meta") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(v.clone()),
                };
                let record = store::new_record(namespace, content, tags, meta);
                let timestamp_ms = record.timestamp_ms;
                store::append(&record).map_err(|e| (-32000, e.to_string()))?;
                Ok(serde_json::json!({"ok": true, "timestamp_ms": timestamp_ms}))
            }
            "memory.query" => {
                Self::require_permission(inbox, Permission::MemoryRead, "memory.read").await?;
                let namespace = Self::param_str(&params, "namespace")?;
                Self::require_namespace_matches(inbox, &namespace).await?;
                let q = MemoryQuery {
                    content_contains: params
                        .get("content_contains")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    tag_prefix: params
                        .get("tag_prefix")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    since_ms: params.get("since_ms").and_then(Value::as_u64),
                    until_ms: params.get("until_ms").and_then(Value::as_u64),
                    limit: params
                        .get("limit")
                        .and_then(Value::as_u64)
                        .map(|n| n as usize),
                };
                let records = store::query(&namespace, &q).map_err(|e| (-32000, e.to_string()))?;
                Ok(serde_json::json!({"records": records}))
            }
            other => Err((-32601, format!("method not found: {other}"))),
        }
    }

    async fn require_permission(
        inbox: &Arc<Inbox>,
        perm: crate::extensions::permissions::Permission,
        wire: &str,
    ) -> Result<(), (i32, String)> {
        let guard = inbox.permissions.read().await;
        match guard.as_ref() {
            Some(set) if set.has(perm) => Ok(()),
            _ => Err((
                -32602,
                format!("permission denied: {wire} required"),
            )),
        }
    }

    async fn require_namespace_matches(
        inbox: &Arc<Inbox>,
        namespace: &str,
    ) -> Result<(), (i32, String)> {
        if namespace == inbox.extension_id {
            Ok(())
        } else {
            Err((
                -32602,
                format!(
                    "namespace must equal extension id '{}' (got '{}')",
                    inbox.extension_id, namespace
                ),
            ))
        }
    }

    fn param_str(params: &Value, name: &str) -> Result<String, (i32, String)> {
        params
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| (-32602, format!("missing or invalid '{name}' parameter")))
    }

    pub async fn initialize(&self, plugin_root: Option<PathBuf>, config: Value) -> Result<InitializeCapabilitiesResult, String> {
        let params = InitializeParams {
            synaps_version: env!("CARGO_PKG_VERSION"),
            extension_protocol_version: CURRENT_EXTENSION_PROTOCOL_VERSION,
            plugin_id: self.id.clone(),
            plugin_root: plugin_root
                .or_else(|| self.cwd.clone())
                .map(|path| path.to_string_lossy().to_string()),
            config,
        };
        let value = self.call_no_restart("initialize", serde_json::to_value(params).map_err(|e| e.to_string())?).await?;
        Self::parse_initialize_result(&self.id, value)
    }

    fn parse_initialize_result(id: &str, value: Value) -> Result<InitializeCapabilitiesResult, String> {
        let result: InitializeResult = serde_json::from_value(value)
            .map_err(|e| format!("Invalid initialize response from extension '{}': {}", id, e))?;
        if result.protocol_version != CURRENT_EXTENSION_PROTOCOL_VERSION {
            return Err(format!(
                "Extension '{}' initialize returned unsupported protocol_version {} (supported: {})",
                id, result.protocol_version, CURRENT_EXTENSION_PROTOCOL_VERSION,
            ));
        }
        Self::validate_registered_tool_specs(id, &result.capabilities.tools)?;
        Self::validate_registered_provider_specs(id, &result.capabilities.providers)?;
        Ok(InitializeCapabilitiesResult {
            tools: result.capabilities.tools,
            providers: result.capabilities.providers,
            voice: result.capabilities.voice,
        })
    }

    fn validate_registered_tool_specs(id: &str, tools: &[RegisteredExtensionToolSpec]) -> Result<(), String> {
        use crate::extensions::validation::{validate_id_segment, IdValidationError};
        let mut names = HashSet::new();
        for tool in tools {
            let name = tool.name.trim();
            if let Err(err) = validate_id_segment(name) {
                return Err(match err {
                    IdValidationError::Empty => format!(
                        "Extension '{}' registered a tool with an empty tool name",
                        id
                    ),
                    IdValidationError::ContainsReserved { ch } => format!(
                        "Extension '{}' registered tool '{}' with invalid tool name: '{}' is reserved",
                        id, name, ch
                    ),
                    IdValidationError::TooLong { len, max } => format!(
                        "Extension '{}' registered tool '{}' with invalid tool name: must be at most {} chars (got {})",
                        id, name, max, len
                    ),
                    IdValidationError::ContainsWhitespace => format!(
                        "Extension '{}' registered tool '{}' with invalid tool name: must not contain whitespace",
                        id, name
                    ),
                });
            }
            if !names.insert(name.to_string()) {
                return Err(format!("Extension '{}' registered duplicate tool name '{}'", id, name));
            }
            if tool.description.trim().is_empty() {
                return Err(format!(
                    "Extension '{}' registered tool '{}' with an empty description",
                    id, name,
                ));
            }
            if !tool.input_schema.is_object() {
                return Err(format!(
                    "Extension '{}' registered tool '{}' with invalid input_schema: input_schema must be a JSON object",
                    id, name,
                ));
            }
        }
        Ok(())
    }

    fn validate_registered_provider_specs(id: &str, providers: &[RegisteredProviderSpec]) -> Result<(), String> {
        use crate::extensions::validation::{validate_id_segment, IdValidationError};
        for provider in providers {
            let provider_id = provider.id.trim();
            match validate_id_segment(provider_id) {
                Ok(()) => {
                    if !Self::is_safe_provider_id(provider_id) {
                        return Err(format!(
                            "Extension '{}' registered provider '{}' with invalid provider id",
                            id, provider_id
                        ));
                    }
                }
                Err(IdValidationError::Empty) => {
                    return Err(format!(
                        "Extension '{}' registered provider with empty provider id",
                        id
                    ));
                }
                Err(err) => {
                    return Err(format!(
                        "Extension '{}' registered provider '{}' with invalid provider id: {}",
                        id, provider_id, err
                    ));
                }
            }
            if provider.display_name.trim().is_empty() {
                return Err(format!(
                    "Extension '{}' registered provider '{}' with empty display_name",
                    id, provider_id,
                ));
            }
            if provider.description.trim().is_empty() {
                return Err(format!(
                    "Extension '{}' registered provider '{}' with empty description",
                    id, provider_id,
                ));
            }
            if provider.models.is_empty() {
                return Err(format!(
                    "Extension '{}' registered provider '{}' must declare at least one model",
                    id, provider_id,
                ));
            }
            let mut model_ids = HashSet::new();
            for model in &provider.models {
                let model_id = model.id.trim();
                if let Err(err) = validate_id_segment(model_id) {
                    return Err(match err {
                        IdValidationError::Empty => format!(
                            "Extension '{}' registered provider '{}' with empty model id",
                            id, provider_id
                        ),
                        IdValidationError::ContainsReserved { ch } => format!(
                            "Extension '{}' registered provider '{}' with invalid model id '{}': '{}' is reserved",
                            id, provider_id, model_id, ch
                        ),
                        IdValidationError::TooLong { len, max } => format!(
                            "Extension '{}' registered provider '{}' with invalid model id '{}': must be at most {} chars (got {})",
                            id, provider_id, model_id, max, len
                        ),
                        IdValidationError::ContainsWhitespace => format!(
                            "Extension '{}' registered provider '{}' with invalid model id '{}': must not contain whitespace",
                            id, provider_id, model_id
                        ),
                    });
                }
                if !model_ids.insert(model_id.to_string()) {
                    return Err(format!(
                        "Extension '{}' registered provider '{}' with duplicate model id '{}'",
                        id, provider_id, model_id,
                    ));
                }
            }
            if let Some(config_schema) = &provider.config_schema {
                if !config_schema.is_object() {
                    return Err(format!(
                        "Extension '{}' registered provider '{}' with invalid config_schema: config_schema must be a JSON object",
                        id, provider_id,
                    ));
                }
            }
        }
        Ok(())
    }

    fn is_safe_provider_id(id: &str) -> bool {
        !id.is_empty()
            && !id.contains(':')
            && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    }

    #[doc(hidden)]
    pub async fn initialize_for_test(&self, plugin_root: Option<PathBuf>) -> Result<(), String> {
        self.initialize(plugin_root, Value::Object(Default::default())).await.map(|_| ())
    }

    async fn restart_locked(&self, state: &mut Option<ProcessState>) -> Result<(), String> {
        let attempted = self.restart_count.fetch_add(1, Ordering::Relaxed) + 1;
        let max_attempts = self.restart_policy.max_attempts;
        if attempted > max_attempts as usize {
            *state = None;
            return Err(format!(
                "Extension '{}' exceeded restart limit ({})",
                self.id, max_attempts,
            ));
        }

        if let Some(old) = state.take() {
            old.reader_handle.abort();
            let mut child = old.child;
            let _ = child.kill().await;
        }
        // Drain any stale pending entries before reusing the inbox.
        self.inbox
            .fail_all_pending("transport closed: process restarting")
            .await;

        let delay = self
            .restart_policy
            .delay_for_attempt(attempted as u32)
            .unwrap_or_default();

        tracing::warn!(
            extension = %self.id,
            attempt = attempted,
            max_attempts = max_attempts,
            delay_ms = delay.as_millis() as u64,
            "Restarting extension process after transport failure",
        );

        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }

        *state = Some(Self::spawn_state(
            &self.id,
            &self.command,
            &self.args,
            self.cwd.as_ref(),
            self.inbox.clone(),
        ).await?);
        self.initialize_locked(state).await?;
        Ok(())
    }


    async fn initialize_locked(&self, state: &mut Option<ProcessState>) -> Result<(), String> {
        let params = InitializeParams {
            synaps_version: env!("CARGO_PKG_VERSION"),
            extension_protocol_version: CURRENT_EXTENSION_PROTOCOL_VERSION,
            plugin_id: self.id.clone(),
            plugin_root: self.cwd
                .clone()
                .map(|path| path.to_string_lossy().to_string()),
            config: Value::Object(Default::default()),
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let value = self.call_once_locked(
            state.as_mut().expect("state should exist for initialize"),
            "initialize",
            serde_json::to_value(params).map_err(|e| e.to_string())?,
            id,
        ).await?;
        Self::parse_initialize_result(&self.id, value).map(|_| ())
    }

    /// Send a single JSON-RPC request and await the matching response,
    /// using the shared inbox for response delivery. The reader task runs
    /// concurrently and may dispatch interleaved notifications.
    async fn call_once_locked(
        &self,
        state: &mut ProcessState,
        method: &str,
        params: Value,
        id: u64,
    ) -> Result<Value, String> {
        let body = serde_json::to_string(&JsonRpcRequest {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
            id,
        })
        .map_err(|e| format!("Serialize error: {}", e))?;

        let (tx, rx) = oneshot::channel::<Result<Value, String>>();
        // Register pending BEFORE writing so the reader can route a fast
        // response without racing against the insert.
        self.inbox.pending.lock().await.insert(id, tx);

        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let write_result = {
            let mut stdin = state.stdin.lock().await;
            match stdin.write_all(frame.as_bytes()).await {
                Ok(()) => stdin.flush().await,
                Err(e) => Err(e),
            }
        };
        if let Err(e) = write_result {
            // Make sure we don't leak the pending entry if the write fails.
            self.inbox.pending.lock().await.remove(&id);
            return Err(format!("Write error: {}", e));
        }

        match rx.await {
            Ok(payload) => payload,
            Err(_) => {
                // Sender was dropped without sending — typically because the
                // reader task observed EOF/error after we registered. The
                // reader normally sends an Err first; this branch is a
                // belt-and-braces fallback.
                self.inbox.pending.lock().await.remove(&id);
                Err("transport closed: response channel dropped".to_string())
            }
        }
    }

    async fn call_no_restart(&self, method: &str, params: Value) -> Result<Value, String> {
        let _call_guard = self.call_lock.lock().await;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut state_guard = self.state.lock().await;
        if state_guard.is_none() {
            *state_guard = Some(Self::spawn_state(
                &self.id,
                &self.command,
                &self.args,
                self.cwd.as_ref(),
                self.inbox.clone(),
            ).await?);
        }
        self.call_once_locked(
            state_guard.as_mut().expect("state should exist"),
            method,
            params,
            id,
        ).await
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let _call_guard = self.call_lock.lock().await;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut state_guard = self.state.lock().await;
        if state_guard.is_none() {
            self.restart_locked(&mut state_guard).await?;
        }

        let result = self
            .call_once_locked(
                state_guard.as_mut().expect("state should exist after restart"),
                method,
                params.clone(),
                id,
            )
            .await;

        match result {
            Ok(value) => Ok(value),
            Err(first_error) => {
                self.restart_locked(&mut state_guard).await?;
                let retry_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                self.call_once_locked(
                    state_guard.as_mut().expect("state should exist after restart"),
                    method,
                    params,
                    retry_id,
                )
                .await
                .map_err(|retry_error| {
                    format!("{}; retry after restart failed: {}", first_error, retry_error)
                })
            }
        }
    }

    /// Subscribe to JSON-RPC notifications emitted by the extension.
    ///
    /// Returns an unbounded receiver that will yield every notification
    /// frame (no `id`, has `method`) the extension sends until either:
    /// - the receiver is dropped,
    /// - the reader observes EOF or a transport error, or
    /// - another caller calls `subscribe_notifications`, in which case the
    ///   previous subscriber's sender is dropped (only one subscription is
    ///   supported at a time).
    ///
    /// Internal API: exposed publicly with `#[doc(hidden)]` only so
    /// integration tests can exercise the bidirectional transport.
    #[doc(hidden)]
    pub async fn subscribe_notifications(&self) -> mpsc::UnboundedReceiver<NotificationFrame> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut sink = self.inbox.notification_sink.lock().await;
        *sink = Some(tx);
        rx
    }

    /// Drop the current notification subscription, if any.
    #[doc(hidden)]
    pub async fn unsubscribe_notifications(&self) {
        self.inbox.notification_sink.lock().await.take();
    }

    /// Forward one notification frame received during `provider.stream`.
    ///
    /// - Frames whose method is not `provider.stream.event` are ignored (logged at trace).
    /// - Malformed event params are logged at warn and skipped (do not abort the call).
    /// - If the caller's sink has been closed, sets `sink_open = false` and stops forwarding,
    ///   but the in-flight request is still allowed to complete.
    fn forward_provider_stream_frame(
        extension_id: &str,
        sink: &mpsc::UnboundedSender<ProviderStreamEvent>,
        sink_open: &mut bool,
        frame: NotificationFrame,
    ) {
        if frame.method != "provider.stream.event" {
            tracing::trace!(
                extension = %extension_id,
                method = %frame.method,
                "Ignoring non-stream notification during provider.stream",
            );
            return;
        }
        match parse_provider_stream_event(&frame.params) {
            Ok(event) => {
                if *sink_open && sink.send(event).is_err() {
                    *sink_open = false;
                }
            }
            Err(error) => {
                tracing::warn!(
                    extension = %extension_id,
                    error = %error,
                    params = %frame.params,
                    "Skipping malformed provider.stream.event notification",
                );
            }
        }
    }
}

#[async_trait]
impl ExtensionHandler for ProcessExtension {
    fn id(&self) -> &str {
        &self.id
    }

    async fn call_tool(&self, name: &str, input: Value) -> Result<Value, String> {
        self.call("tool.call", serde_json::json!({
            "name": name,
            "input": input,
        })).await
    }

    async fn provider_complete(&self, params: ProviderCompleteParams) -> Result<ProviderCompleteResult, String> {
        let value = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            self.call("provider.complete", serde_json::to_value(params).map_err(|e| e.to_string())?),
        )
        .await
        .map_err(|_| format!("Extension '{}' provider.complete timed out", self.id))??;
        let result: ProviderCompleteResult = serde_json::from_value(value)
            .map_err(|e| format!("Invalid provider.complete response from extension '{}': {}", self.id, e))?;
        if result.content.is_empty() {
            return Err(format!("Extension '{}' provider.complete returned empty content", self.id));
        }
        Ok(result)
    }

    async fn provider_stream(
        &self,
        params: ProviderCompleteParams,
        sink: tokio::sync::mpsc::UnboundedSender<ProviderStreamEvent>,
    ) -> Result<ProviderCompleteResult, String> {
        // Subscribe BEFORE issuing the request so we don't miss early
        // notifications that may arrive before `call(...)` even starts polling.
        let mut rx = self.subscribe_notifications().await;
        let params_value =
            serde_json::to_value(params).map_err(|e| e.to_string())?;

        let extension_id = self.id.clone();
        let stream_future = async {
            let mut call_fut = Box::pin(self.call("provider.stream", params_value));
            let mut sink_open = true;
            let response = loop {
                tokio::select! {
                    response = &mut call_fut => break response,
                    Some(frame) = rx.recv() => {
                        Self::forward_provider_stream_frame(
                            &extension_id, &sink, &mut sink_open, frame,
                        );
                    }
                }
            };
            // Response received: clear the inbox's notification sender so the
            // receiver yields `None` once buffered frames are drained, then
            // flush any remaining notifications before returning.
            self.unsubscribe_notifications().await;
            while let Some(frame) = rx.recv().await {
                Self::forward_provider_stream_frame(
                    &extension_id, &sink, &mut sink_open, frame,
                );
            }
            response
        };

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            stream_future,
        )
        .await;

        // Belt-and-braces: ensure the subscription is cleared on timeout too.
        self.unsubscribe_notifications().await;

        let value = outcome
            .map_err(|_| format!("Extension '{}' provider.stream timed out", self.id))??;

        let result: ProviderCompleteResult = serde_json::from_value(value)
            .map_err(|e| {
                format!("Invalid provider.stream response from extension '{}': {}", self.id, e)
            })?;
        // NOTE: empty `content` is permitted for streaming — output may have
        // been delivered entirely via TextDelta notifications.
        Ok(result)
    }

    async fn handle(&self, event: &HookEvent) -> HookResult {
        let params = serde_json::to_value(event).unwrap_or(Value::Null);
        match tokio::time::timeout(std::time::Duration::from_secs(5), self.call("hook.handle", params)).await {
            Ok(Ok(value)) => match serde_json::from_value(value.clone()) {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(
                        extension = %self.id,
                        error = %error,
                        response = %value,
                        "Extension hook handler returned invalid result",
                    );
                    if value.get("action").and_then(Value::as_str) == Some("modify") {
                        HookResult::Block {
                            reason: "Extension returned malformed modify result".to_string(),
                        }
                    } else {
                        HookResult::Continue
                    }
                }
            },
            Ok(Err(e)) => {
                tracing::warn!(
                    extension = %self.id,
                    error = %e,
                    "Extension hook handler failed — continuing",
                );
                HookResult::Continue
            }
            Err(_) => {
                tracing::warn!(
                    extension = %self.id,
                    timeout_secs = 5,
                    "Extension hook handler timed out — continuing",
                );
                HookResult::Continue
            }
        }
    }

    async fn shutdown(&self) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            self.call("shutdown", Value::Null),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let mut state_guard = self.state.lock().await;
        if let Some(state) = state_guard.take() {
            state.reader_handle.abort();
            let mut child = state.child;
            let _ = child.kill().await;
        }
        // Drop any active notification subscriber and signal pending callers.
        self.inbox.notification_sink.lock().await.take();
        self.inbox
            .fail_all_pending("transport closed: extension shutdown")
            .await;
    }

    async fn restart_count(&self) -> usize {
        self.restart_count()
    }

    async fn health(&self) -> ExtensionHealth {
        let count = self.restart_count.load(Ordering::Relaxed);
        let max = self.restart_policy.max_attempts as usize;
        if count >= max {
            ExtensionHealth::Failed
        } else if count > 0 {
            // Within budget. If the state slot is currently empty, we're
            // mid-restart; otherwise the process is alive but has previously
            // crashed at least once.
            let state_alive = self.state.try_lock().map(|g| g.is_some()).unwrap_or(true);
            if state_alive {
                ExtensionHealth::Degraded
            } else {
                ExtensionHealth::Restarting
            }
        } else {
            ExtensionHealth::Running
        }
    }
}

#[cfg(test)]
mod stream_event_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_text_delta_with_delta_key() {
        let v = json!({"type": "text", "delta": "hi"});
        assert_eq!(
            parse_provider_stream_event(&v).unwrap(),
            ProviderStreamEvent::TextDelta { text: "hi".into() }
        );
    }

    #[test]
    fn parses_text_delta_with_text_key() {
        let v = json!({"type": "text", "text": "hi"});
        assert_eq!(
            parse_provider_stream_event(&v).unwrap(),
            ProviderStreamEvent::TextDelta { text: "hi".into() }
        );
    }

    #[test]
    fn parses_thinking_delta() {
        let v = json!({"type": "thinking", "delta": "hmm"});
        assert_eq!(
            parse_provider_stream_event(&v).unwrap(),
            ProviderStreamEvent::ThinkingDelta { text: "hmm".into() }
        );
        let v2 = json!({"type": "thinking", "text": "hmm"});
        assert_eq!(
            parse_provider_stream_event(&v2).unwrap(),
            ProviderStreamEvent::ThinkingDelta { text: "hmm".into() }
        );
    }

    #[test]
    fn parses_tool_use() {
        let v = json!({
            "type": "tool_use",
            "id": "t1",
            "name": "echo",
            "input": {"x": 1}
        });
        assert_eq!(
            parse_provider_stream_event(&v).unwrap(),
            ProviderStreamEvent::ToolUse {
                id: "t1".into(),
                name: "echo".into(),
                input: json!({"x": 1}),
            }
        );
    }

    #[test]
    fn tool_use_input_defaults_to_empty_object() {
        let v = json!({"type": "tool_use", "id": "t1", "name": "echo"});
        assert_eq!(
            parse_provider_stream_event(&v).unwrap(),
            ProviderStreamEvent::ToolUse {
                id: "t1".into(),
                name: "echo".into(),
                input: json!({}),
            }
        );
    }

    #[test]
    fn parses_usage_strips_type() {
        let v = json!({"type": "usage", "input_tokens": 5, "output_tokens": 7});
        assert_eq!(
            parse_provider_stream_event(&v).unwrap(),
            ProviderStreamEvent::Usage {
                usage: json!({"input_tokens": 5, "output_tokens": 7})
            }
        );
    }

    #[test]
    fn parses_error() {
        let v = json!({"type": "error", "message": "boom"});
        assert_eq!(
            parse_provider_stream_event(&v).unwrap(),
            ProviderStreamEvent::Error { message: "boom".into() }
        );
    }

    #[test]
    fn parses_done() {
        let v = json!({"type": "done"});
        assert_eq!(
            parse_provider_stream_event(&v).unwrap(),
            ProviderStreamEvent::Done
        );
    }

    #[test]
    fn nested_event_shape_matches_flat() {
        let flat = json!({"type": "text", "delta": "hi"});
        let nested = json!({"event": {"type": "text", "delta": "hi"}});
        assert_eq!(
            parse_provider_stream_event(&flat).unwrap(),
            parse_provider_stream_event(&nested).unwrap()
        );
    }

    #[test]
    fn missing_type_errors() {
        let v = json!({"delta": "hi"});
        let err = parse_provider_stream_event(&v).unwrap_err();
        assert!(err.contains("missing type"), "got: {err}");
    }

    #[test]
    fn unknown_type_errors_with_type() {
        let v = json!({"type": "wat"});
        let err = parse_provider_stream_event(&v).unwrap_err();
        assert!(err.contains("wat"), "got: {err}");
    }

    #[test]
    fn tool_use_missing_id_errors() {
        let v = json!({"type": "tool_use", "name": "echo"});
        let err = parse_provider_stream_event(&v).unwrap_err();
        assert!(err.contains("id"), "got: {err}");
    }

    #[test]
    fn tool_use_missing_name_errors() {
        let v = json!({"type": "tool_use", "id": "t1"});
        let err = parse_provider_stream_event(&v).unwrap_err();
        assert!(err.contains("name"), "got: {err}");
    }

    #[test]
    fn tool_use_empty_id_errors() {
        let v = json!({"type": "tool_use", "id": "", "name": "echo"});
        assert!(parse_provider_stream_event(&v).is_err());
    }

    #[test]
    fn tool_use_empty_name_errors() {
        let v = json!({"type": "tool_use", "id": "t1", "name": ""});
        assert!(parse_provider_stream_event(&v).is_err());
    }

    #[test]
    fn tool_use_non_object_input_errors() {
        let v = json!({"type": "tool_use", "id": "t1", "name": "echo", "input": "nope"});
        let err = parse_provider_stream_event(&v).unwrap_err();
        assert!(err.contains("input"), "got: {err}");
    }

    #[test]
    fn text_missing_delta_and_text_errors() {
        let v = json!({"type": "text"});
        let err = parse_provider_stream_event(&v).unwrap_err();
        assert!(err.contains("delta") || err.contains("text"), "got: {err}");
    }

    #[test]
    fn error_missing_message_errors() {
        let v = json!({"type": "error"});
        assert!(parse_provider_stream_event(&v).is_err());
    }

    #[test]
    fn error_empty_message_errors() {
        let v = json!({"type": "error", "message": ""});
        assert!(parse_provider_stream_event(&v).is_err());
    }
}

#[cfg(test)]
mod restart_policy_tests {
    use super::*;

    #[tokio::test]
    async fn restart_policy_default_max_attempts_is_3() {
        // Use a command that won't actually run; we only need the struct.
        // Since `spawn` actually launches the process, use a trivial echoer
        // and immediately shut it down. Falls back to /bin/cat which reads
        // from stdin and stays alive.
        let ext = ProcessExtension::spawn("policy-test", "/bin/cat", &[])
            .await
            .expect("spawn /bin/cat");
        assert_eq!(ext.restart_policy.max_attempts, 3);
        ext.shutdown().await;
    }

    #[tokio::test]
    async fn with_restart_policy_overrides_default() {
        let ext = ProcessExtension::spawn("policy-test-override", "/bin/cat", &[])
            .await
            .expect("spawn /bin/cat");
        let custom = RestartPolicy {
            max_attempts: 7,
            ..RestartPolicy::default()
        };
        let ext = ext.with_restart_policy(custom);
        assert_eq!(ext.restart_policy.max_attempts, 7);
        ext.shutdown().await;
    }
}

#[cfg(test)]
mod voice_validator_tests {
    use super::*;
    use crate::extensions::permissions::{Permission, PermissionSet};

    fn perms_with(grants: &[Permission]) -> PermissionSet {
        let mut p = PermissionSet::new();
        for g in grants {
            p.grant(*g);
        }
        p
    }

    fn decl(name: &str, modes: &[&str]) -> VoiceCapabilityDeclaration {
        VoiceCapabilityDeclaration {
            name: name.to_string(),
            modes: modes.iter().map(|m| m.to_string()).collect(),
            endpoint: None,
        }
    }

    #[test]
    fn voice_validator_rejects_empty_name() {
        let d = decl("   ", &["stt"]);
        let perms = perms_with(&[Permission::AudioInput]);
        let err = validate_voice_capability(&d, &perms).unwrap_err();
        assert!(err.contains("name"), "got: {}", err);
    }

    #[test]
    fn voice_validator_rejects_empty_modes() {
        let d = decl("Whisper", &[]);
        let perms = perms_with(&[Permission::AudioInput]);
        let err = validate_voice_capability(&d, &perms).unwrap_err();
        assert!(err.contains("modes"), "got: {}", err);
    }

    #[test]
    fn voice_validator_rejects_unknown_mode() {
        let d = decl("Whisper", &["humming"]);
        let perms = perms_with(&[Permission::AudioInput, Permission::AudioOutput]);
        let err = validate_voice_capability(&d, &perms).unwrap_err();
        assert!(err.contains("unknown mode"), "got: {}", err);
    }

    #[test]
    fn voice_validator_requires_audio_input_for_stt() {
        let d = decl("Whisper", &["stt"]);
        let perms = perms_with(&[]);
        let err = validate_voice_capability(&d, &perms).unwrap_err();
        assert!(err.contains("audio.input"), "got: {}", err);
    }

    #[test]
    fn voice_validator_requires_audio_output_for_tts() {
        let d = decl("Piper", &["tts"]);
        let perms = perms_with(&[Permission::AudioInput]);
        let err = validate_voice_capability(&d, &perms).unwrap_err();
        assert!(err.contains("audio.output"), "got: {}", err);
    }

    #[test]
    fn voice_validator_accepts_valid_stt_with_permission() {
        let d = decl("Whisper", &["stt"]);
        let perms = perms_with(&[Permission::AudioInput]);
        validate_voice_capability(&d, &perms).expect("should validate");
    }

    #[test]
    fn voice_validator_accepts_combined_modes_with_both_permissions() {
        let d = decl("Voice", &["stt", "tts", "wake_word"]);
        let perms = perms_with(&[Permission::AudioInput, Permission::AudioOutput]);
        validate_voice_capability(&d, &perms).expect("should validate");
    }

    #[test]
    fn voice_validator_wake_word_requires_audio_input() {
        let d = decl("Porcupine", &["wake_word"]);
        let perms = perms_with(&[]);
        let err = validate_voice_capability(&d, &perms).unwrap_err();
        assert!(err.contains("audio.input"), "got: {}", err);
    }
}
