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
mod dir;
mod placeholders;
mod plan;

use super::{resolve_path_in, Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde::Deserialize;
use serde_json::{json, Value};
use std::ffi::OsString;
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

fn parse_request(params: &Value) -> std::result::Result<Request, String> {
    let obj = params
        .as_object()
        .ok_or("parameters must be an object with an `edits` list")?;
    for key in obj.keys() {
        if key != "edits" && key != "placeholders" {
            return Err(format!("unknown parameter `{key}`"));
        }
    }
    let placeholders = match obj.get("placeholders") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("`placeholders` must be true or false".into()),
    };
    let raw = obj
        .get("edits")
        .and_then(Value::as_array)
        .ok_or("`edits` must be a list")?;
    if raw.is_empty() {
        return Err("`edits` is empty".into());
    }
    if raw.len() > MAX_EDITS {
        return Err(format!("{} edits; at most {MAX_EDITS} per call", raw.len()));
    }
    let mut edits = Vec::with_capacity(raw.len());
    for (i, value) in raw.iter().enumerate() {
        let spec: EditSpec =
            serde_json::from_value(value.clone()).map_err(|e| format!("edits[{i}]: {e}"))?;
        check_shape(&spec).map_err(|e| format!("edits[{i}] ({}): {e}", spec.path))?;
        edits.push(spec);
    }
    Ok(Request {
        edits,
        placeholders,
    })
}

/// Field combinations and sizes, before anything is read.
fn check_shape(spec: &EditSpec) -> std::result::Result<(), String> {
    if spec.path.trim().is_empty() {
        return Err("path is empty".into());
    }
    let op = spec.op.as_str();
    if spec.op.anchored() {
        match spec.anchor.as_deref() {
            None => return Err(format!("{op} needs an anchor")),
            Some("") => return Err("anchor is empty".into()),
            Some(_) => {}
        }
        if spec.create == Some(true) {
            return Err("create is only for append/append_jsonl".into());
        }
    } else if spec.anchor.is_some() || spec.anchor_mode.is_some() || spec.occurrence.is_some() {
        return Err(format!("{op} takes no anchor, anchor_mode or occurrence"));
    }
    if spec.occurrence == Some(0) {
        return Err("occurrence is 1-based".into());
    }
    if spec.op == Op::AppendJsonl {
        if spec.text.is_some() {
            return Err("append_jsonl takes `value` (a JSON object), not `text`".into());
        }
        if spec.value.is_none() {
            return Err("append_jsonl needs `value` (a JSON object)".into());
        }
        if spec.timestamp_field.as_deref() == Some("") {
            return Err("timestamp_field is empty".into());
        }
    } else {
        if spec.value.is_some() || spec.timestamp_field.is_some() {
            return Err(format!("{op} takes `text`, not `value`/`timestamp_field`"));
        }
        match spec.text.as_deref() {
            None => return Err(format!("{op} needs `text`")),
            Some("") if spec.op != Op::Replace => {
                return Err(format!("{op} with empty text changes nothing"))
            }
            _ => {}
        }
    }
    if spec.unless_present.as_deref() == Some("") {
        return Err("unless_present is empty (it would always match)".into());
    }
    let sizes = [
        spec.anchor.as_ref().map_or(0, String::len),
        spec.text.as_ref().map_or(0, String::len),
        spec.unless_present.as_ref().map_or(0, String::len),
        spec.value
            .as_ref()
            .map_or(0, |v| serde_json::to_string(v).map_or(usize::MAX, |s| s.len())),
    ];
    if sizes.iter().any(|&n| n > MAX_TEXT_BYTES) {
        return Err(format!("a field exceeds {MAX_TEXT_BYTES} bytes"));
    }
    Ok(())
}

/// Worker write scopes: refused when the path, as given, as resolved, or as
/// the real target (symlinks followed), overlaps an active worker's scope.
pub(crate) type ScopeCheck<'a> = &'a dyn Fn(&str) -> bool;

/// One file touched by the call, with its in-memory content.
struct FileEntry {
    display: String,
    target: PathBuf,
    /// The verified parent directory of `target`, opened right after the
    /// scope check from the same canonical path.
    dir: dir::Dir,
    /// The file name of `target` in `dir`.
    name: OsString,
    original: Option<String>,
    identity: Option<(u64, u64)>,
    mode: Option<u32>,
    content: String,
    /// The "(created)" note was already shown for this new file.
    created_noted: bool,
}

/// Where `raw` really writes: `(resolved, target, exists)`. An existing path
/// resolves through symlinks to its target; a missing one is the canonical
/// parent plus the file name.
fn resolve_target(raw: &str, cwd: Option<&Path>) -> std::result::Result<(PathBuf, PathBuf, bool), String> {
    let path = resolve_path_in(raw, cwd);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            let target = std::fs::canonicalize(&path).map_err(|e| {
                if meta.file_type().is_symlink() {
                    format!("dangling symlink ({e})")
                } else {
                    format!("cannot resolve the path: {e}")
                }
            })?;
            Ok((path, target, true))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let name = path
                .file_name()
                .ok_or("path has no file name")?
                .to_os_string();
            let parent = match path.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                _ => PathBuf::from("."),
            };
            let parent = std::fs::canonicalize(&parent)
                .map_err(|e| format!("parent directory {} is not usable: {e}", parent.display()))?;
            if !parent.is_dir() {
                return Err(format!("parent {} is not a directory", parent.display()));
            }
            Ok((path, parent.join(name), false))
        }
        Err(e) => Err(format!("cannot inspect the path: {e}")),
    }
}

/// The verified parent directory of a resolved target, and the target's
/// file name in it. Opened from the canonical path the scope check saw.
fn open_parent(target: &Path) -> std::result::Result<(dir::Dir, OsString), String> {
    let name = target.file_name().ok_or("path has no file name")?.to_os_string();
    let parent = target.parent().ok_or("path has no parent directory")?;
    let dir = dir::Dir::open(parent)
        .map_err(|e| format!("parent directory {} is not usable: {e}", parent.display()))?;
    Ok((dir, name))
}

/// An existing file as read during planning.
struct Loaded {
    content: String,
    identity: Option<(u64, u64)>,
    mode: Option<u32>,
}

/// Reads `name` through its parent's handle: never by a path that could
/// lead elsewhere. The open never waits (a FIFO with no writer, a device)
/// and the type is checked on the descriptor before anything is read.
#[cfg_attr(not(test), allow(unused_variables))]
fn load_existing(
    dir: &dir::Dir,
    name: &std::ffi::OsStr,
    hooks: &mut dyn commit::Hooks,
) -> std::result::Result<Loaded, String> {
    use std::io::Read;
    let (file, meta) = dir.open_regular(name).map_err(|e| match e {
        dir::OpenError::NotRegular => "not a regular file".to_string(),
        dir::OpenError::Symlink => "was replaced by a symlink since it was resolved".to_string(),
        dir::OpenError::Io(e) => format!("cannot open: {e}"),
    })?;
    if meta.nlink > 1 {
        return Err(format!(
            "has {} hard links; replacing it would split them",
            meta.nlink
        ));
    }
    if meta.size > MAX_FILE_BYTES as u64 {
        return Err(format!("larger than {MAX_FILE_BYTES} bytes"));
    }
    #[cfg(test)]
    hooks.before_read();
    let mut bytes = Vec::with_capacity(meta.size as usize);
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read: {e}"))?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!("larger than {MAX_FILE_BYTES} bytes"));
    }
    let text = String::from_utf8(bytes).map_err(|_| "not UTF-8 text".to_string())?;
    Ok(Loaded {
        content: text,
        identity: meta.identity,
        mode: meta.mode,
    })
}

/// Plan and commit one call. Synchronous: run it off the async runtime.
fn run(
    req: Request,
    now: chrono::DateTime<chrono::Local>,
    cwd: Option<&Path>,
    scope_refuses: ScopeCheck<'_>,
    hooks: &mut dyn commit::Hooks,
) -> std::result::Result<String, String> {
    let mut expander = placeholders::Expander::new(now);
    let uses_clock = req.placeholders
        || req.edits.iter().any(|e| e.timestamp_field.is_some());
    let mut files: Vec<FileEntry> = Vec::new();
    let mut index_of: std::collections::HashMap<PathBuf, usize> = Default::default();
    let mut lines: Vec<String> = Vec::with_capacity(req.edits.len());
    let (mut applied, mut skipped) = (0usize, 0usize);

    for (i, spec) in req.edits.iter().enumerate() {
        let op = spec.op.as_str();
        let at = |e: String| format!("edits[{i}] ({}, {op}): {e}; no file was changed", spec.path);
        let (resolved, target, exists) = resolve_target(&spec.path, cwd).map_err(at)?;
        for candidate in [
            spec.path.clone(),
            resolved.to_string_lossy().into_owned(),
            target.to_string_lossy().into_owned(),
        ] {
            if scope_refuses(&candidate) {
                return Err(at(format!(
                    "{candidate} overlaps an active worker's write scope"
                )));
            }
        }
        let slot = match index_of.get(&target) {
            Some(&slot) => slot,
            None => {
                if files.len() == MAX_FILES {
                    return Err(at(format!("more than {MAX_FILES} files in one call")));
                }
                let (dir, name) = open_parent(&target).map_err(at)?;
                let entry = if exists {
                    let Loaded {
                        content,
                        identity,
                        mode,
                    } = load_existing(&dir, &name, hooks).map_err(at)?;
                    FileEntry {
                        display: spec.path.clone(),
                        target: target.clone(),
                        dir,
                        name,
                        original: Some(content.clone()),
                        identity,
                        mode,
                        content,
                        created_noted: false,
                    }
                } else {
                    if !(matches!(spec.op, Op::Append | Op::AppendJsonl)
                        && spec.create == Some(true))
                    {
                        return Err(at(
                            "file does not exist (only append/append_jsonl with create: true may create one)".into(),
                        ));
                    }
                    FileEntry {
                        display: spec.path.clone(),
                        target: target.clone(),
                        dir,
                        name,
                        original: None,
                        identity: None,
                        mode: None,
                        content: String::new(),
                        created_noted: false,
                    }
                };
                files.push(entry);
                index_of.insert(target.clone(), files.len() - 1);
                files.len() - 1
            }
        };

        let expand = |expander: &mut placeholders::Expander, s: &str| {
            if req.placeholders {
                expander.expand(s)
            } else {
                Ok(s.to_string())
            }
        };
        let guard = match &spec.unless_present {
            Some(g) => Some(expand(&mut expander, g).map_err(at)?),
            None => None,
        };
        let text = if spec.op == Op::AppendJsonl {
            let mut row = Value::Object(spec.value.clone().unwrap_or_default());
            if req.placeholders {
                expander.expand_value(&mut row).map_err(at)?;
            }
            if let Some(field) = &spec.timestamp_field {
                let map = row.as_object_mut().expect("value is an object");
                if map.contains_key(field) {
                    return Err(at(format!("timestamp_field `{field}` is already in value")));
                }
                map.insert(field.clone(), Value::String(expander.now_iso()));
            }
            serde_json::to_string(&row).map_err(|e| at(e.to_string()))?
        } else {
            expand(&mut expander, spec.text.as_deref().unwrap_or_default()).map_err(at)?
        };
        if text.len() > MAX_TEXT_BYTES {
            return Err(at(format!("expanded text exceeds {MAX_TEXT_BYTES} bytes")));
        }

        let entry = &mut files[slot];
        let outcome = plan::apply(
            &mut entry.content,
            spec.op,
            spec.anchor_mode.unwrap_or_default(),
            spec.anchor.as_deref(),
            spec.occurrence,
            &text,
            guard.as_deref(),
        )
        .map_err(at)?;
        if entry.content.len() > MAX_FILE_BYTES {
            return Err(at(format!("the file would exceed {MAX_FILE_BYTES} bytes")));
        }
        lines.push(match outcome {
            plan::Applied::Skipped { line } => {
                skipped += 1;
                format!("[{i}] {} {op} skipped: unless_present found at L{line}", spec.path)
            }
            plan::Applied::Edit {
                first_line,
                last_line,
                removed,
                added,
                note,
            } => {
                applied += 1;
                let span = if first_line == last_line {
                    format!("L{first_line}")
                } else {
                    format!("L{first_line}-{last_line}")
                };
                let bytes = if spec.op == Op::Replace {
                    format!("-{removed} +{added} B")
                } else {
                    format!("+{added} B")
                };
                let mut line = format!("[{i}] {} {op} {span} {bytes}", spec.path);
                if entry.original.is_none() && !entry.created_noted {
                    entry.created_noted = true;
                    line.push_str(" (created)");
                }
                if let Some(note) = note {
                    line.push_str(&format!(" ({note})"));
                }
                line
            }
        });
    }

    let total: usize = files.iter().map(|f| f.content.len()).sum();
    if total > MAX_TOTAL_BYTES {
        return Err(format!(
            "the call would write {total} bytes; at most {MAX_TOTAL_BYTES}; no file was changed"
        ));
    }
    let pending: Vec<commit::Pending> = files
        .into_iter()
        .filter(|f| f.original.as_deref() != Some(f.content.as_str()))
        .map(|f| commit::Pending {
            display: f.display,
            target: f.target,
            dir: f.dir,
            name: f.name,
            original: f.original.map(String::into_bytes),
            identity: f.identity,
            mode: f.mode,
            new: f.content.into_bytes(),
        })
        .collect();
    if !pending.is_empty() {
        commit::commit(&pending, hooks)?;
    }

    let mut out = format!(
        "multi_edit: {} file(s) written, {applied} edit(s) applied, {skipped} skipped",
        pending.len()
    );
    if uses_clock {
        out.push_str(&format!("; clock {}", expander.now_iso()));
    }
    out.push_str(" (line numbers are as of each edit)\n");
    out.push_str(&lines.join("\n"));
    if !pending.is_empty() {
        let written: Vec<String> = pending
            .iter()
            .map(|p| p.target.display().to_string())
            .collect();
        out.push_str(&format!("\nwritten: {}", written.join(", ")));
    }
    Ok(out)
}

/// The distinct paths a `multi_edit` call names, in first-seen order, as
/// given (for compaction's file list and status labels). Tolerates any
/// input shape: anything that is not `edits[].path` strings is ignored.
pub(crate) fn paths(input: &Value) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for edit in input["edits"].as_array().into_iter().flatten() {
        if let Some(path) = edit["path"].as_str() {
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// One-line status: `editing a.md` or `editing a.md +2 files`.
pub(crate) fn label(input: &Value) -> String {
    let paths = paths(input);
    let short = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    match paths.as_slice() {
        [] => "editing ?".to_string(),
        [one] => format!("editing {}", short(one)),
        [first, rest @ ..] => format!("editing {} +{} files", short(first), rest.len()),
    }
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

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let req = parse_request(&params).map_err(RuntimeError::Tool)?;
        let now = chrono::Local::now();
        let cwd = ctx.capabilities.cwd.clone();
        let orchestration = ctx.capabilities.orchestration.clone();
        tokio::task::spawn_blocking(move || {
            let refuses = |path: &str| {
                orchestration.as_ref().is_some_and(|o| {
                    matches!(
                        o.check_foreground_write(path),
                        agent_core::orchestration::ScopeDecision::ReconciliationRequired { .. }
                    )
                })
            };
            run(req, now, cwd.as_deref(), &refuses, &mut commit::RealHooks)
        })
        .await
        .map_err(|e| RuntimeError::Tool(format!("multi_edit task failed: {e}")))?
        .map_err(RuntimeError::Tool)
    }
}

#[cfg(test)]
mod tests;
