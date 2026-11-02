//! Memory-only tool results (docs/memory-only-results.md).
//!
//! A marked tool result never enters the durable history: the history keeps a
//! fixed placeholder, and the raw text lives only in this RAM table, owned by
//! the live session. It is swapped in on the provider request COPY, so every
//! record built from the history (session file, journal, archives, summaries,
//! reload/crash records, views) sees only the placeholder. Lifetime ends at
//! restart, reload, park, rollover, compaction, a new session, eviction, or
//! when the call id is no longer in the history. Nothing is replayed: the
//! model calls the tool again.
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use serde::Serialize;
use serde_json::Value;

use crate::tools::{ToolOrigin, ToolOutput};
use crate::SharedMessage;

/// Per-result marker a tool result object may carry: `_meta[META_KEY] == true`.
pub const META_KEY: &str = "synaps/memory_only";
/// One result over this many bytes is refused, never truncated.
pub const PER_RESULT_CAP: usize = 262_144;
/// Live raw bytes per session; the oldest results are evicted to fit.
pub const SESSION_CAP: usize = 1_048_576;

const PREFIX: &str = "[memory-only result of ";
const KEPT_SUFFIX: &str =
    " is not kept in this session's records. Call the tool again if you need it.]";
const REFUSED_SUFFIX: &str = " Nothing was kept. Ask for less and call again.]";
const WITHHELD_PREFIX: &str = "[memory-only result withheld: max_tool_output ";
const WITHHELD_SUFFIX: &str = " Nothing was kept. Raise max_tool_output and call again.]";

/// The fixed text the history keeps instead of a memory-only result.
pub fn placeholder(tool: &str, call_id: &str) -> String {
    format!("{PREFIX}{tool} (call {call_id}){KEPT_SUFFIX}")
}

/// The fixed text the history keeps for a refused (oversize) result.
pub fn refusal(tool: &str, call_id: &str, bytes: usize) -> String {
    format!(
        "{PREFIX}{tool} (call {call_id}) refused: {bytes} bytes is over the {PER_RESULT_CAP}-byte limit for one result.{REFUSED_SUFFIX}"
    )
}

/// The fixed text the history keeps when the placeholder itself is over
/// `max_tool_output`: the history would keep it truncated, and a truncated
/// placeholder is never swapped back, so nothing is held and the result
/// says why. The reason comes first, so a truncated copy still says it.
pub fn withheld(tool: &str, call_id: &str, max_tool_output: usize) -> String {
    format!(
        "{WITHHELD_PREFIX}({max_tool_output} bytes) is too small for the placeholder of {tool} (call {call_id}).{WITHHELD_SUFFIX}"
    )
}

/// True for exactly the engine's placeholder / refusal / withheld shape.
pub fn is_marker(text: &str) -> bool {
    let shaped = |prefix: &str, suffixes: &[&str]| {
        text.starts_with(prefix)
            && text.contains(" (call ")
            && !text.contains('\n')
            && suffixes.iter().any(|suffix| text.ends_with(suffix))
    };
    shaped(PREFIX, &[KEPT_SUFFIX, REFUSED_SUFFIX]) || shaped(WITHHELD_PREFIX, &[WITHHELD_SUFFIX])
}

/// True when a tool result object carries `_meta["synaps/memory_only"] == true`.
pub fn meta_marked(value: &Value) -> bool {
    value
        .get("_meta")
        .and_then(|meta| meta.get(META_KEY))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `*` matches any run of characters, `?` exactly one; everything else is literal.
pub fn glob_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    // The last `*` seen and the name position it is currently stretched to.
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || (p[pi] != '*' && p[pi] == n[ni])) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// A session's own memory-only globs: ONE list per session, shared by every
/// runtime, stream, tool call and worker of that session, and read at each
/// mark check, so a live `Set` reaches a turn already in flight and workers
/// already running. It only grows ([`SessionGlobs::add`]), so sharing it
/// with running work only ever adds marking.
#[derive(Clone, Default)]
pub struct SessionGlobs {
    inner: Arc<RwLock<SessionList>>,
}

#[derive(Default)]
struct SessionList {
    globs: Vec<String>,
    refused: u64,
}

impl std::fmt::Debug for SessionGlobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.globs()).finish()
    }
}

impl SessionGlobs {
    /// A new list holding `globs` (duplicates dropped).
    pub fn new(globs: Vec<String>) -> Self {
        let list = Self::default();
        let _ = list.add(&globs);
        list
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, SessionList> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn globs(&self) -> Vec<String> {
        self.read().globs.clone()
    }

    /// Add-only update. `globs` must hold every glob already on the list:
    /// the new ones are added (`Ok(added)`; an identical list adds none).
    /// A list that leaves any out is refused: `Err(the left-out globs)`,
    /// nothing changes, and the refusal is counted.
    pub fn add(&self, globs: &[String]) -> Result<usize, Vec<String>> {
        let mut list = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let dropped: Vec<String> = list.globs.iter().filter(|g| !globs.contains(g)).cloned().collect();
        if !dropped.is_empty() {
            list.refused += 1;
            return Err(dropped);
        }
        let before = list.globs.len();
        for glob in globs {
            if !list.globs.contains(glob) {
                list.globs.push(glob.clone());
            }
        }
        Ok(list.globs.len() - before)
    }

    /// How many updates were refused for dropping a glob.
    pub fn refused(&self) -> u64 {
        self.read().refused
    }

    fn any(&self, hit: impl Fn(&str) -> bool) -> bool {
        self.read().globs.iter().any(|glob| hit(glob.as_str()))
    }
}

/// Which tools are memory-only by name: globs from config plus the session's
/// shared list. Only extension and MCP tools can be marked (built-ins are
/// out of v1).
#[derive(Clone, Debug, Default)]
pub struct Policy {
    config: Vec<String>,
    session: SessionGlobs,
}

impl Policy {
    pub fn set_config(&mut self, globs: Vec<String>) {
        self.config = globs;
    }
    /// A NEW session list (a session's create; tests). A clone of this
    /// policy shares it.
    pub fn set_session(&mut self, globs: Vec<String>) {
        self.session = SessionGlobs::new(globs);
    }
    /// Read `session`'s list from now on, at each check.
    pub fn share_session(&mut self, session: &SessionGlobs) {
        self.session = session.clone();
    }
    pub fn session(&self) -> &SessionGlobs {
        &self.session
    }
    /// A worker: its caller's config globs join its own, and it reads its
    /// caller's session list live (an addition made while it runs marks
    /// its later calls).
    pub fn inherit(&mut self, caller: &Policy) {
        for glob in &caller.config {
            if !self.config.contains(glob) {
                self.config.push(glob.clone());
            }
        }
        self.session = caller.session.clone();
    }
    /// Every glob in force now (config, then the session's), once each.
    pub fn globs(&self) -> Vec<String> {
        let mut all = self.config.clone();
        for glob in self.session.globs() {
            if !all.contains(&glob) {
                all.push(glob);
            }
        }
        all
    }
    /// A glob matches any of three names: `runtime_name`, the registry name
    /// (`<extension_id>:<tool>`, `ext__<server>__<tool>`); `wire_name`, the
    /// name the model sees (`<extension_id>_<tool>`); and the tool's OWN
    /// name, as its extension or MCP server registered it (`<tool>`).
    pub fn marks(&self, origin: &ToolOrigin, runtime_name: &str, wire_name: &str) -> bool {
        let own = match origin {
            ToolOrigin::Extension { extension_id } => runtime_name
                .strip_prefix(extension_id.as_str())
                .and_then(|rest| rest.strip_prefix(':'))
                .unwrap_or(runtime_name),
            ToolOrigin::Mcp { server_tool_name, .. } => server_tool_name.as_str(),
            _ => return false,
        };
        let hit = |glob: &str| {
            glob_matches(glob, runtime_name) || glob_matches(glob, wire_name) || glob_matches(glob, own)
        };
        self.config.iter().any(|glob| hit(glob)) || self.session.any(hit)
    }
}

struct Entry {
    call_id: String,
    placeholder: String,
    raw: String,
}

#[derive(Default)]
struct Inner {
    entries: VecDeque<Entry>,
    live_bytes: usize,
    evicted: u64,
    refused: u64,
}

/// Counts only; never the raw text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    pub live: usize,
    pub live_bytes: usize,
    pub per_result_cap: usize,
    pub session_cap: usize,
    pub evicted: u64,
    pub refused: u64,
}

/// What a tool call produced once memory-only marking is applied.
pub enum Taken {
    /// Memory-only: the durable text (placeholder or refusal).
    Kept(String),
    /// Not memory-only: the result, untouched.
    Plain(crate::Result<ToolOutput>),
}

/// The live raw results of ONE session. Shared explicitly with that
/// session's stream; `Runtime::clone` makes a fresh, empty table.
#[derive(Clone, Default)]
pub struct Store {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MemoryOnlyStore")
    }
}

impl Store {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hold `raw` for this call and return the durable placeholder, or count
    /// a refusal and return the refusal text when `raw` is over the cap.
    pub fn keep(&self, tool: &str, call_id: &str, raw: String) -> String {
        let bytes = raw.len();
        let mut s = self.lock();
        if bytes > PER_RESULT_CAP {
            s.refused += 1;
            return refusal(tool, call_id, bytes);
        }
        // A repeated call id replaces its earlier entry.
        if let Some(pos) = s.entries.iter().position(|e| e.call_id == call_id) {
            if let Some(old) = s.entries.remove(pos) {
                s.live_bytes -= old.raw.len();
            }
        }
        // Over the session cap: the oldest live results go back to their
        // placeholder (one result under the per-result cap always fits).
        while s.live_bytes + bytes > SESSION_CAP {
            let Some(old) = s.entries.pop_front() else { break };
            s.live_bytes -= old.raw.len();
            s.evicted += 1;
        }
        let text = placeholder(tool, call_id);
        s.live_bytes += bytes;
        s.entries.push_back(Entry { call_id: call_id.to_string(), placeholder: text.clone(), raw });
        text
    }

    /// Apply memory-only marking to one finished call. Marked when the policy
    /// matched the tool's name or the tool returned [`ToolOutput::MemoryOnly`];
    /// a marked call's error text is memory-only too. A placeholder over
    /// `max_tool_output` would be truncated in the history and never swapped
    /// back, so then nothing is held and the result is the [`withheld`]
    /// notice instead (fail closed, but not silently).
    pub fn take(
        &self,
        policy_marked: bool,
        tool: &str,
        call_id: &str,
        res: crate::Result<ToolOutput>,
        max_tool_output: usize,
    ) -> Taken {
        let raw = match res {
            Ok(ToolOutput::MemoryOnly(raw)) => raw,
            Ok(output) if policy_marked => output.into_summary(),
            Err(e) if policy_marked => e.to_string(),
            res => return Taken::Plain(res),
        };
        if placeholder(tool, call_id).len() > max_tool_output {
            tracing::debug!(tool, call_id, max_tool_output, "memory-only result withheld: its placeholder is over max_tool_output");
            return Taken::Kept(withheld(tool, call_id, max_tool_output));
        }
        Taken::Kept(self.keep(tool, call_id, raw))
    }

    /// Build the provider request copy: every tool_result whose content is a
    /// live placeholder gets its raw text back. First releases entries whose
    /// placeholder is no longer in `durable`. `None` when nothing was swapped.
    pub fn substitute(
        &self,
        durable: &[SharedMessage],
        request: &[SharedMessage],
    ) -> Option<Vec<SharedMessage>> {
        let mut s = self.lock();
        if s.entries.is_empty() {
            return None;
        }
        let in_history: HashSet<(&str, &str)> = tool_results(durable).collect();
        let before = s.entries.len();
        let mut released = 0;
        s.entries.retain(|e| {
            let keep = in_history.contains(&(e.call_id.as_str(), e.placeholder.as_str()));
            if !keep {
                released += e.raw.len();
            }
            keep
        });
        s.live_bytes -= released;
        if s.entries.len() != before {
            tracing::debug!(released = before - s.entries.len(), "memory-only results released: no longer in the history");
        }
        if s.entries.is_empty() {
            return None;
        }
        let mut swapped = false;
        let out = request
            .iter()
            .map(|msg| match swap_message(msg, &s.entries) {
                Some(copy) => {
                    swapped = true;
                    Arc::new(copy)
                }
                None => Arc::clone(msg),
            })
            .collect();
        swapped.then_some(out)
    }

    /// Drop every live raw result (counters are kept).
    pub fn clear(&self) {
        let mut s = self.lock();
        s.entries.clear();
        s.live_bytes = 0;
    }

    pub fn has_live(&self) -> bool {
        !self.lock().entries.is_empty()
    }

    pub fn status(&self) -> Status {
        let s = self.lock();
        Status {
            live: s.entries.len(),
            live_bytes: s.live_bytes,
            per_result_cap: PER_RESULT_CAP,
            session_cap: SESSION_CAP,
            evicted: s.evicted,
            refused: s.refused,
        }
    }
}

/// Every `(tool_use_id, text content)` of the tool_result blocks in `messages`.
fn tool_results(messages: &[SharedMessage]) -> impl Iterator<Item = (&str, &str)> {
    messages
        .iter()
        .filter_map(|msg| msg.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|block| {
            Some((
                block.get("tool_use_id")?.as_str()?,
                block.get("content")?.as_str()?,
            ))
        })
}

/// A copy of `msg` with each live placeholder swapped for its raw text, or
/// `None` when the message holds no live placeholder.
fn swap_message(msg: &Value, entries: &VecDeque<Entry>) -> Option<Value> {
    let blocks = msg.get("content")?.as_array()?;
    if !blocks.iter().any(|b| raw_for(b, entries).is_some()) {
        return None;
    }
    let mut copy = msg.clone();
    if let Some(out) = copy.get_mut("content").and_then(Value::as_array_mut) {
        for (block, original) in out.iter_mut().zip(blocks) {
            if let Some(raw) = raw_for(original, entries) {
                block["content"] = Value::String(raw.to_string());
            }
        }
    }
    Some(copy)
}

/// The live raw text for one tool_result block whose content is still
/// exactly its call's placeholder.
fn raw_for<'a>(block: &Value, entries: &'a VecDeque<Entry>) -> Option<&'a str> {
    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
        return None;
    }
    let id = block.get("tool_use_id")?.as_str()?;
    let content = block.get("content")?.as_str()?;
    entries
        .iter()
        .find(|e| e.call_id == id && e.placeholder == content)
        .map(|e| e.raw.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ext(id: &str) -> ToolOrigin {
        ToolOrigin::Extension { extension_id: id.into() }
    }

    fn mcp() -> ToolOrigin {
        ToolOrigin::Mcp { server_id: "kag".into(), server_tool_name: "search".into() }
    }

    fn result_msg(id: &str, content: &str) -> SharedMessage {
        Arc::new(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": content, "cache_control": {"type": "ephemeral"}}
        ]}))
    }

    #[test]
    fn memory_only_texts_are_fixed_and_recognised() {
        let kept = placeholder("vault_fetch", "toolu_1");
        assert_eq!(
            kept,
            "[memory-only result of vault_fetch (call toolu_1) is not kept in this session's records. Call the tool again if you need it.]"
        );
        let refused = refusal("vault_fetch", "toolu_1", 300_000);
        assert_eq!(
            refused,
            "[memory-only result of vault_fetch (call toolu_1) refused: 300000 bytes is over the 262144-byte limit for one result. Nothing was kept. Ask for less and call again.]"
        );
        assert!(is_marker(&kept));
        assert!(is_marker(&refused));
        let withheld = withheld("vault_fetch", "toolu_1", 60);
        assert_eq!(
            withheld,
            "[memory-only result withheld: max_tool_output (60 bytes) is too small for the placeholder of vault_fetch (call toolu_1). Nothing was kept. Raise max_tool_output and call again.]"
        );
        assert!(is_marker(&withheld));
        assert!(!is_marker("[memory-only result withheld: max_tool_output (60 bytes)"));
        assert!(!is_marker("[memory-only result of x (call y) is not kept"));
        assert!(!is_marker(&format!("{kept}\nmore")));
        assert!(!is_marker("ordinary output"));
    }

    #[test]
    fn memory_only_meta_flag_must_be_true() {
        assert!(meta_marked(&json!({"content": "x", "_meta": {"synaps/memory_only": true}})));
        assert!(!meta_marked(&json!({"content": "x", "_meta": {"synaps/memory_only": "true"}})));
        assert!(!meta_marked(&json!({"content": "x", "_meta": {"synaps/memory_only": false}})));
        assert!(!meta_marked(&json!({"content": "x"})));
        assert!(!meta_marked(&json!("x")));
    }

    #[test]
    fn memory_only_glob_matching() {
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("memfix:*", "memfix:vault_fetch"));
        assert!(glob_matches("memfix_*", "memfix_vault_fetch"));
        assert!(glob_matches("ext__kag__*", "ext__kag__search"));
        assert!(glob_matches("a?c", "abc"));
        assert!(glob_matches("*fetch*", "memfix:vault_fetch_all"));
        assert!(!glob_matches("a?c", "ac"));
        assert!(!glob_matches("memfix:*", "other:vault_fetch"));
        assert!(!glob_matches("memfix", "memfix:vault_fetch"));
        assert!(glob_matches("", ""));
        assert!(!glob_matches("", "x"));
    }

    #[test]
    fn memory_only_policy_marks_extension_and_mcp_by_either_name_never_builtins() {
        let mut policy = Policy::default();
        policy.set_config(vec!["ext__kag__*".into()]);
        policy.set_session(vec!["memfix:*".into(), "pria-vault_*".into()]);
        assert_eq!(policy.globs(), vec!["ext__kag__*", "memfix:*", "pria-vault_*"]);
        // Extension `memfix` tool `vault_fetch`: runtime name `memfix:vault_fetch`,
        // wire name `memfix_vault_fetch`. The runtime-name glob matches.
        assert!(policy.marks(&ext("memfix"), "memfix:vault_fetch", "memfix_vault_fetch"));
        // Extension `pria-vault` tool `get`: the wire-name glob matches.
        assert!(policy.marks(&ext("pria-vault"), "pria-vault:get", "pria-vault_get"));
        assert!(policy.marks(&mcp(), "ext__kag__search", "ext__kag__search"));
        assert!(!policy.marks(&ext("other"), "other:get", "other_get"));
        let mut star = Policy::default();
        star.set_session(vec!["*".into()]);
        assert!(!star.marks(&ToolOrigin::Builtin, "read", "read"));
        assert!(!star.marks(&ToolOrigin::Unknown, "x", "x"));
        assert!(!star.marks(
            &ToolOrigin::Plugin { plugin_id: "p".into(), tool_name: "t".into() },
            "p:t",
            "p_t"
        ));
        assert!(star.marks(&ext("memfix"), "memfix:vault_fetch", "memfix_vault_fetch"));
        assert!(!Policy::default().marks(&ext("memfix"), "memfix:x", "memfix_x"));
    }

    /// 790d's ingress seam: an MCP tool is marked by its server tool name.
    #[test]
    fn memory_only_policy_marks_an_mcp_tool_by_its_server_tool_name() {
        let mcp = |tool: &str| ToolOrigin::Mcp { server_id: "pria".into(), server_tool_name: tool.into() };
        let mut policy = Policy::default();
        policy.set_session(vec!["pria_vault_*".into(), "pria_memory_*".into()]);
        assert!(policy.marks(&mcp("pria_vault_get"), "ext__pria__pria_vault_get", "ext__pria__pria_vault_get"));
        assert!(policy.marks(&mcp("pria_memory_search"), "ext__pria__pria_memory_search", "ext__pria__pria_memory_search"));
        assert!(!policy.marks(&mcp("other_get"), "ext__pria__other_get", "ext__pria__other_get"));
    }

    /// 790d's add-only Set: an identical resend and a superset succeed, a
    /// list that drops a glob is refused, counted, and changes nothing.
    #[test]
    fn memory_only_session_globs_are_add_only() {
        let ik1: Vec<String> = vec!["pria_vault_*".into(), "pria_memory_*".into()];
        let list = SessionGlobs::new(ik1.clone());
        let mut policy = Policy::default();
        policy.share_session(&list);
        // An identical resend: ok, nothing changes.
        assert_eq!(list.add(&ik1), Ok(0));
        assert_eq!(list.globs(), ik1);
        // A superset adds the new globs.
        let wider: Vec<String> = vec!["pria_vault_*".into(), "pria_memory_*".into(), "pria_extra_*".into()];
        assert_eq!(list.add(&wider), Ok(1));
        assert_eq!(list.globs(), wider);
        // Dropping any glob is refused, names the dropped globs, is counted.
        assert_eq!(list.add(&ik1), Err(vec!["pria_extra_*".to_string()]));
        assert_eq!(list.add(&[]), Err(wider.clone()));
        assert_eq!(list.refused(), 2);
        // Unchanged after the refusals, and still marking.
        assert_eq!(list.globs(), wider);
        assert!(policy.marks(&ext("pria-desktop"), "pria-desktop:pria_extra_get", "pria-desktop_pria_extra_get"));
        assert_eq!(policy.globs(), wider);
        assert_eq!(SessionGlobs::new(vec!["a".into(), "a".into()]).globs(), vec!["a"]);
    }

    /// The session list is one shared handle read at each check: a stream's
    /// clone and a running worker see an addition made after they started.
    #[test]
    fn memory_only_session_globs_are_shared_and_read_at_each_check() {
        let list = SessionGlobs::new(Vec::new());
        let mut session = Policy::default();
        session.set_config(vec!["cfg:*".into()]);
        session.share_session(&list);
        let stream = session.clone();
        let mut worker = Policy::default();
        worker.set_config(vec!["own:*".into()]);
        worker.inherit(&stream);
        let vault = |p: &Policy| p.marks(&ext("pria-desktop"), "pria-desktop:pria_vault_get", "pria-desktop_pria_vault_get");
        assert!(!vault(&stream) && !vault(&worker));
        assert_eq!(list.add(&["pria_vault_*".to_string()]), Ok(1));
        assert!(vault(&session) && vault(&stream) && vault(&worker), "an addition reaches work already running");
        assert_eq!(worker.globs(), vec!["own:*", "cfg:*", "pria_vault_*"]);
        // A new list (another session's create) is not shared.
        let mut other = session.clone();
        other.set_session(Vec::new());
        assert!(!vault(&other));
        assert!(vault(&session));
    }

    #[test]
    fn memory_only_store_keeps_raw_in_ram_and_swaps_it_into_the_request_copy() {
        let store = Store::default();
        let kept = store.keep("vault_fetch", "toolu_1", "SECRET-RAW".into());
        assert_eq!(kept, placeholder("vault_fetch", "toolu_1"));
        assert!(store.has_live());
        let durable = vec![
            Arc::new(json!({"role": "user", "content": "hi"})) as SharedMessage,
            result_msg("toolu_1", &kept),
        ];
        let request = store.substitute(&durable, &durable).expect("swapped");
        assert_eq!(request[1]["content"][0]["content"], "SECRET-RAW");
        assert_eq!(request[1]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(request[1]["content"][0]["tool_use_id"], "toolu_1");
        assert!(Arc::ptr_eq(&request[0], &durable[0]));
        // The durable history itself is untouched.
        assert_eq!(durable[1]["content"][0]["content"], kept.as_str());
        let status = store.status();
        assert_eq!((status.live, status.live_bytes), (1, 10));
        assert!(!format!("{store:?}").contains("SECRET"));
    }

    #[test]
    fn memory_only_store_refuses_oversize_never_truncates() {
        let store = Store::default();
        let raw = "x".repeat(PER_RESULT_CAP + 1);
        let text = store.keep("vault_fetch", "toolu_big", raw);
        assert_eq!(text, refusal("vault_fetch", "toolu_big", PER_RESULT_CAP + 1));
        assert!(!store.has_live());
        assert_eq!(store.status().refused, 1);
        let fits = store.keep("vault_fetch", "toolu_ok", "y".repeat(PER_RESULT_CAP));
        assert_eq!(fits, placeholder("vault_fetch", "toolu_ok"));
        assert_eq!(store.status().live_bytes, PER_RESULT_CAP);
    }

    #[test]
    fn memory_only_store_evicts_oldest_over_the_session_cap() {
        let store = Store::default();
        let mut durable = Vec::new();
        for i in 0..5 {
            let id = format!("toolu_{i}");
            let kept = store.keep("t", &id, format!("{i}").repeat(PER_RESULT_CAP));
            durable.push(result_msg(&id, &kept));
        }
        let status = store.status();
        assert_eq!((status.live, status.evicted), (4, 1));
        assert_eq!(status.live_bytes, SESSION_CAP);
        let request = store.substitute(&durable, &durable).expect("swapped");
        // The oldest stays a placeholder; the newest four are raw.
        assert_eq!(request[0]["content"][0]["content"], placeholder("t", "toolu_0").as_str());
        assert_eq!(request[4]["content"][0]["content"].as_str().unwrap().len(), PER_RESULT_CAP);
    }

    #[test]
    fn memory_only_store_releases_results_gone_from_history_and_never_replays() {
        let store = Store::default();
        let kept = store.keep("t", "toolu_1", "RAW-ONE".into());
        let _ = store.keep("t", "toolu_2", "RAW-TWO".into());
        let durable = vec![result_msg("toolu_1", &kept)];
        let request = store.substitute(&durable, &durable).expect("swapped");
        assert_eq!(request[0]["content"][0]["content"], "RAW-ONE");
        let status = store.status();
        assert_eq!((status.live, status.live_bytes), (1, 7));
        // A history that no longer holds the placeholder releases the result,
        // and a later history holding it again gets no raw back.
        assert!(store.substitute(&[], &[]).is_none());
        assert!(!store.has_live());
        assert!(store.substitute(&durable, &durable).is_none());
        // A request carrying text that merely equals the placeholder under a
        // different id gets nothing.
        let other = store.keep("t", "toolu_3", "RAW-THREE".into());
        let spoof = vec![result_msg("toolu_4", &other)];
        assert!(store.substitute(&spoof, &spoof).is_none());
        store.clear();
        assert_eq!(store.status().live, 0);
    }

    #[test]
    fn memory_only_take_marks_by_policy_or_by_tool_output() {
        let store = Store::default();
        match store.take(false, "t", "c1", Ok(ToolOutput::MemoryOnly("RAW".into())), 30_000) {
            Taken::Kept(text) => assert_eq!(text, placeholder("t", "c1")),
            Taken::Plain(_) => panic!("a MemoryOnly output is always kept"),
        }
        match store.take(true, "t", "c2", Ok(ToolOutput::Text("RAW2".into())), 30_000) {
            Taken::Kept(text) => assert_eq!(text, placeholder("t", "c2")),
            Taken::Plain(_) => panic!("a policy-marked output is kept"),
        }
        match store.take(true, "t", "c3", Err(crate::RuntimeError::Tool("RAW-ERR".into())), 30_000) {
            Taken::Kept(text) => assert_eq!(text, placeholder("t", "c3")),
            Taken::Plain(_) => panic!("a policy-marked error is kept"),
        }
        match store.take(false, "t", "c4", Ok(ToolOutput::Text("plain".into())), 30_000) {
            Taken::Plain(Ok(o)) => assert_eq!(o.summary(), "plain"),
            _ => panic!("unmarked output passes through"),
        }
        assert_eq!(store.status().live, 3);
    }

    #[test]
    fn memory_only_take_withholds_when_the_placeholder_is_over_max_tool_output() {
        let store = Store::default();
        let fits = placeholder("vault_fetch", "toolu_1").len();
        // Exactly the placeholder's length: it fits, so the raw is held.
        match store.take(false, "vault_fetch", "toolu_1", Ok(ToolOutput::MemoryOnly("SECRET-RAW".into())), fits) {
            Taken::Kept(text) => assert_eq!(text, placeholder("vault_fetch", "toolu_1")),
            Taken::Plain(_) => panic!("a MemoryOnly output is always kept"),
        }
        assert_eq!(store.status().live, 1);
        // One byte under: withheld, nothing held, and the notice says why.
        let under = placeholder("vault_fetch", "toolu_2").len() - 1;
        for (call, res) in [
            ("toolu_2", Ok(ToolOutput::MemoryOnly("SECRET-RAW".into()))),
            ("toolu_3", Err(crate::RuntimeError::Tool("SECRET-RAW".into()))),
        ] {
            match store.take(true, "vault_fetch", call, res, under) {
                Taken::Kept(text) => {
                    assert_eq!(text, withheld("vault_fetch", call, under));
                    assert!(text.starts_with("[memory-only result withheld: max_tool_output "), "{text}");
                    assert!(text.contains("too small") && !text.contains("SECRET-RAW"), "{text}");
                    assert!(is_marker(&text));
                }
                Taken::Plain(_) => panic!("a marked call is never plain"),
            }
        }
        assert_eq!(store.status().live, 1, "nothing is held for a withheld result");
        assert_eq!(store.status().live_bytes, "SECRET-RAW".len());
        // Unmarked output is untouched whatever max_tool_output is.
        assert!(matches!(store.take(false, "t", "c", Ok(ToolOutput::Text("plain".into())), 1), Taken::Plain(Ok(_))));
    }

    #[test]
    fn memory_only_clone_shares_and_default_is_fresh() {
        let store = Store::default();
        let _ = store.keep("t", "c", "RAW".into());
        assert!(store.clone().has_live());
        assert!(!Store::default().has_live());
    }
}
