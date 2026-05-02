# Phase I slice 1 implementation notes

Prepared while the Phase H verification agent was still running.

## Narrow scope

Implement only the append-only local session metadata index described in `docs/specs/local-first-session-memory-index.md`:

- New module: `src/core/session_index.rs`
- Export from `src/core/mod.rs`
- Storage path: `$SYNAPS_BASE_DIR/sessions/index.jsonl`
- JSONL only; one JSON object per line
- Required fields:
  - `schema_version: 1`
  - `session_id`
  - `event: "start" | "end"`
  - `timestamp` as UTC RFC3339
- Optional fields omitted when unknown
- No transcript/user/assistant/tool content written
- Failure to append logs warning/debug and does not abort chat

## Likely integration points

In `src/chatui/mod.rs`:

- Around lines 587-590: current `on_session_start` hook emission.
  - Append index `start` record before or near this hook.
  - Available data there: `app.session.id`, `app.session.model`, cwd via `std::env::current_dir()`, active profile via `synaps_cli::core::config::get_profile()`.
- Around lines 1574-1578: current `on_session_end` hook emission.
  - Append index `end` record before or near this hook.
  - Available data there: `app.session.id`, approximate turns via `app.api_messages.len()` if desired.

Do not change hook payloads in slice 1.

## Suggested data types

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionIndexEventKind {
    Start,
    End,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionIndexRecord {
    pub schema_version: u8,
    pub session_id: String,
    pub event: SessionIndexEventKind,
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
```

Keep constructors small, e.g. `SessionIndexRecord::start(session_id)` / `::end(session_id)`.

## Suggested functions

```rust
pub fn index_path() -> PathBuf;
pub fn append_record(record: &SessionIndexRecord) -> crate::Result<()>;
pub fn read_recent(limit: usize) -> crate::Result<Vec<SessionIndexRecord>>;
```

Possible internal helper for easier tests:

```rust
fn append_record_to_path(path: &Path, record: &SessionIndexRecord) -> crate::Result<()>;
fn read_recent_from_path(path: &Path, limit: usize) -> crate::Result<Vec<SessionIndexRecord>>;
```

Use `OpenOptions::new().create(true).append(true)` and create parent directories before append.

## Test plan

Add unit tests in `src/core/session_index.rs`.

Tests that mutate `SYNAPS_BASE_DIR` must use a `static Mutex` guard, matching existing project guidance.

Recommended tests:

1. `append_record_creates_jsonl_under_base_dir`
2. `append_start_and_end_are_valid_json_lines`
3. `read_recent_returns_newest_records_in_chronological_order` or explicitly document newest-first if chosen
4. `read_recent_limit_zero_returns_empty`
5. `append_creates_parent_directories`

Test isolation sketch:

```rust
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvGuard { old: Option<String> }
impl EnvGuard {
    fn set_base_dir(path: &Path) -> Self { ... }
}
impl Drop for EnvGuard { ... }
```

## Verification commands after implementation

Focused first:

```bash
cargo test --bin synaps core::session_index
```

Then relevant integration/focused checks:

```bash
cargo test --test contracts_sync
cargo test --test extensions_contract
cargo test --test extensions_e2e
cargo test --test extensions_process
cargo test --bin synaps chatui::commands::tests::plugin
```

Full suite after `sa_2` completes or when ready to checkpoint:

```bash
cargo test
```
