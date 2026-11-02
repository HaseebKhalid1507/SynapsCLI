//! `tools = confined`: an enforcing session's in-process tools held to its
//! policy too, not only its bash and PTY children.
//!
//! In a session whose file says `mode = enforce` and `tools = confined`:
//! - `write`, `edit` and `multi_edit` refuse a target whose directory is not,
//!   or is not below, one of the policy's directory rules with every write
//!   right (`write`, `infra`, `git_common`'s and `cargo_home`'s directories,
//!   the baseline's `/tmp` and `/dev/shm`), checked on the opened
//!   directory by identity ([`super::protected::ToolGuard`]);
//! - a tool that starts or resumes a worker ([`WORKER_TOOLS`]) is refused;
//! - any other tool runs only if it is on [`BUILTIN_TOOLS`] or named by one
//!   of the file's `tool = <name>` lines.
//!
//! Each refusal is one tool error (with the file's contact sentence) and one
//! `sandbox.tool_refused` line, which names the tool and the class only:
//! never a path or the tool's input. None of this needs kernel support, so
//! it holds whatever the Landlock ABI is; only the kill switch lifts it.

#[cfg(all(test, target_os = "linux"))]
mod tests;

/// The tools a confined session may always run: read-only tools, the tools
/// whose commands run inside the session's Landlock domain, and the three
/// file tools (checked against the write set).
pub const BUILTIN_TOOLS: &[&str] = &[
    "read",
    "grep",
    "find",
    "ls",
    "bash",
    "shell_start",
    "shell_send",
    "shell_end",
    "write",
    "edit",
    "multi_edit",
    "wait",
    "context_checkpoint",
    "memory_search",
    "memory_fetch",
    "forum_read",
];

/// The tools that start or resume a worker. A worker isn't sandboxed, so a
/// confined session refuses them, and no `tool =` line can allow one.
pub const WORKER_TOOLS: &[&str] = &["subagent", "subagent_start", "subagent_resume"];

/// Longest tool name a `tool =` line takes (and a log line prints).
pub const MAX_TOOL_NAME: usize = 128;

/// Why a confined session refused a tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// A file tool's target is outside the session's write set.
    Path,
    /// The tool starts or resumes a worker.
    Worker,
    /// The tool is neither built in nor named by a `tool =` line.
    NotAllowed,
}

impl Class {
    pub fn as_str(&self) -> &'static str {
        match self {
            Class::Path => "path",
            Class::Worker => "worker",
            Class::NotAllowed => "not_allowed",
        }
    }
}

/// A `tool =` value: 1-128 of `A-Z a-z 0-9 _ . : -`.
pub fn valid_tool_name(v: &str) -> bool {
    !v.is_empty() && v.len() <= MAX_TOOL_NAME && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// Whether a confined session with the `tool =` names `named` refuses a call
/// of `tool` (its exact runtime name), and why; `None` = it runs.
pub fn class_of(tool: &str, named: &[String]) -> Option<Class> {
    if WORKER_TOOLS.contains(&tool) {
        return Some(Class::Worker);
    }
    if BUILTIN_TOOLS.contains(&tool) || named.iter().any(|n| n == tool) {
        return None;
    }
    Some(Class::NotAllowed)
}

/// A tool name as a log field: the `tool =` alphabet as is, any other byte
/// as `\xNN`, at most [`MAX_TOOL_NAME`] bytes (then `...`).
fn log_name(tool: &str) -> String {
    let mut out = String::new();
    for b in tool.bytes().take(MAX_TOOL_NAME) {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-') {
            out.push(b as char);
        } else {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    if tool.len() > MAX_TOOL_NAME {
        out.push_str("...");
    }
    if out.is_empty() {
        out.push_str("\\x00");
    }
    out
}

/// The WARN line of one refused call: the tool and the class, never a path
/// or the tool's input.
pub fn refused_line(session: &str, tool: &str, class: Class) -> String {
    format!("sandbox.tool_refused session={session} tool={} class={}", log_name(tool), class.as_str())
}

/// The text a refused call returns to the agent.
pub fn refusal(tool: &str, class: Class, contact: &str) -> String {
    match class {
        Class::Path => format!(
            "Refused by this session's sandbox (tools = confined): `{tool}` may write only inside the directories its policy grants. Nothing was changed. {contact}"
        ),
        Class::Worker => format!(
            "Refused by this session's sandbox (tools = confined): `{tool}` starts or resumes a worker, and a confined session runs none. Nothing was run. {contact}"
        ),
        Class::NotAllowed => format!(
            "Refused by this session's sandbox (tools = confined): `{tool}` is not on its tool list (the built-in read-only and sandboxed tools, and the policy's `tool =` lines). Nothing was run. {contact}"
        ),
    }
}

/// The directories a confined session's file tools may write in (and below):
/// the identity of every directory rule with every write right. File rules
/// (`write_file`, `infra_file`, `device`, `cargo_home`'s files, the
/// baseline's device files) and `/proc` (write-only rights) admit none: the
/// tools replace a file through a temp file and a rename in its directory,
/// which Landlock refuses there too. A rule that can't be statted adds
/// nothing (narrower).
pub fn write_dirs(p: &super::policy::Policy) -> Vec<(u64, u64)> {
    p.rules()
        .iter()
        .filter(|r| r.is_dir() && r.access() == super::landlock::WRITE_RIGHTS)
        .filter_map(|r| r.identity())
        .collect()
}

/// A refused file-tool target: the agent's text names the path as given.
pub fn path_refusal(tool: &str, path: &std::path::Path, contact: &str) -> String {
    format!(
        "Refused by this session's sandbox (tools = confined): `{}` is outside the directories its policy lets `{tool}` write. Nothing was changed. {contact}",
        path.display()
    )
}
