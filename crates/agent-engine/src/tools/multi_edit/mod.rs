//! `multi_edit`: anchored edits across several files, all or nothing.
//!
//! One call carries a list of edits. Every edit is validated and applied to
//! in-memory copies first (`plan`); if any edit fails, no file is written.
//! The changed files are then staged, re-checked and renamed into place
//! (`commit`). Clock/mtime placeholders are opt-in per call
//! (`placeholders`).
//!
//! Effect class: `NonIdempotent`, unlike `edit`/`write`. An interrupted
//! insert/append must never be re-run automatically (it would land twice),
//! and one concurrency key cannot name several files.

mod commit;
mod placeholders;
mod plan;

use super::{resolve_path_in, Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub(crate) const MAX_EDITS: usize = 64;
pub(crate) const MAX_FILES: usize = 16;
pub(crate) const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_TEXT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Op {
    Replace,
    InsertBefore,
    InsertAfter,
    Append,
    AppendJsonl,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Replace => "replace",
            Op::InsertBefore => "insert_before",
            Op::InsertAfter => "insert_after",
            Op::Append => "append",
            Op::AppendJsonl => "append_jsonl",
        }
    }
    fn anchored(self) -> bool {
        matches!(self, Op::Replace | Op::InsertBefore | Op::InsertAfter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AnchorMode {
    #[default]
    Exact,
    LinePrefix,
    LineRegex,
}

/// One edit as sent by the model. Unused optional fields may be omitted or
/// `null`; unknown fields are refused so a typo is never silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditSpec {
    path: String,
    op: Op,
    #[serde(default)]
    anchor: Option<String>,
    #[serde(default)]
    anchor_mode: Option<AnchorMode>,
    #[serde(default)]
    occurrence: Option<u64>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    value: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    timestamp_field: Option<String>,
    #[serde(default)]
    unless_present: Option<String>,
    #[serde(default)]
    create: Option<bool>,
}

struct Request {
    edits: Vec<EditSpec>,
    placeholders: bool,
}

#[allow(dead_code)]
fn parse_request(_params: &Value) -> std::result::Result<Request, String> {
    Err("not implemented".into())
}

pub(crate) type ScopeCheck<'a> = &'a dyn Fn(&str) -> bool;

#[allow(dead_code)]
fn run(
    _req: Request,
    _now: chrono::DateTime<chrono::Local>,
    _cwd: Option<&Path>,
    _scope_refuses: ScopeCheck<'_>,
    _hooks: &mut dyn commit::Hooks,
) -> std::result::Result<String, String> {
    Err("not implemented".into())
}

pub(crate) fn paths(_input: &Value) -> Vec<&str> {
    Vec::new()
}

pub(crate) fn label(_input: &Value) -> String {
    String::new()
}

pub struct MultiEditTool;

#[async_trait::async_trait]
impl Tool for MultiEditTool {
    fn effect(&self) -> crate::tools::catalog::ToolEffect {
        // Not IdempotentWrite: see the module docs.
        crate::tools::catalog::ToolEffect::NonIdempotent
    }

    fn concurrency_key(&self, _validated_input: &Value) -> Option<super::ConcurrencyKey> {
        Some(super::ConcurrencyKey::Serialize)
    }

    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "multi_edit"
    }

    fn description(&self) -> &str {
        "Anchored edits across one or more files, all or nothing: every edit is checked in memory first and no file is written if any fails. Each anchor must match exactly once (or give `occurrence`). Edits to one file apply in order. Returns one line per edit (file, op, lines as of that edit, bytes)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_EDITS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "op": {"type": "string", "enum": ["replace", "insert_before", "insert_after", "append", "append_jsonl"]},
                            "anchor": {"type": "string", "description": "Text to find (replace/insert_*). Literal, never expanded."},
                            "anchor_mode": {"type": "string", "enum": ["exact", "line_prefix", "line_regex"], "description": "exact (default): substring. line_prefix: lines starting with anchor. line_regex: regex per line; with one capture group only the group is the span."},
                            "occurrence": {"type": "integer", "minimum": 1, "description": "Use the Nth match instead of requiring exactly one"},
                            "text": {"type": "string", "description": "Inserted or replacement text, verbatim"},
                            "value": {"type": "object", "description": "append_jsonl: the row object"},
                            "timestamp_field": {"type": "string", "description": "append_jsonl: field set to the call's clock (ISO 8601, local)"},
                            "unless_present": {"type": "string", "description": "Skip this edit if this text is already in the file"},
                            "create": {"type": "boolean", "description": "append/append_jsonl: create the file if missing"}
                        },
                        "required": ["path", "op"]
                    }
                },
                "placeholders": {"type": "boolean", "description": "Expand {{now:FMT}}, {{now_iso}}, {{mtime:FMT:/abs/path}}, {{mtime_iso:/abs/path}} in text, value strings and unless_present; one clock reading per call. {{{{ is a literal {{."}
            },
            "required": ["edits"]
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let _ = parse_request(&params).map_err(RuntimeError::Tool)?;
        Err(RuntimeError::Tool("not implemented".into()))
    }
}

#[cfg(test)]
mod tests;
