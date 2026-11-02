//! The held store (S1, wire contract v1.2 §5):
//! `<base dir>/events/held/<session id>.jsonl`, mode 0600, written by the
//! daemon only, for `pria-fleet held list/show` to read directly.
//!
//! One JSON object per line, folded in order, last one wins:
//! ```text
//! {"v":1,"op":"mode","mode":"hold","lease_until_ms":1761027000000,"ts_ms":...}
//! {"v":1,"op":"item","item":{HeldItem fields + "text"},"event":{the Event as received}}
//! {"v":1,"op":"state","ids":["h-..."],"state":"dismissed","side_session":null,"ts_ms":...}
//! ```
//! Lines with an unknown `op` or a `v` above 1 are skipped, and so is a line
//! that doesn't parse (a torn last line after a crash). Changes are
//! appended; the file is rewritten compacted (a temp file in the same dir,
//! synced, then renamed over it) when it has grown well past its content,
//! when its tail may be torn, or when the 7-day prune dropped something. A
//! compacted file starts with the mode line, then each item in its latest
//! state (a state line after it when it isn't `held`, so the prune keeps its
//! clock).
//!
//! The in-memory fold is the actor's truth; a failed write is logged and
//! the next write rewrites the whole file. A file this image could not read
//! is never rewritten (only appended to), so nothing on disk is lost to it.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::held::*;
use crate::events::types::Event;

/// The store format this image reads and writes.
pub const STORE_VERSION: u64 = 1;
/// Released and dismissed items are pruned this long after their last change.
pub const PRUNE_AFTER_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// A fold brings the pending count down to this, so one fold item covers a
/// batch of events rather than one fold per arrival past the bound.
pub const FOLD_LOW_WATER: usize = HELD_BOUND - 100;

/// One held item with its text and the event it came from (`None` for a
/// fold item, which has no event of its own).
#[derive(Clone, Debug)]
pub struct Entry {
    pub item: HeldItem,
    pub text: String,
    pub event: Option<Event>,
    /// When its state last changed (its arrival until then), ms.
    pub changed_ms: u64,
}

/// What [`HeldStore::add`] folded to stay within [`HELD_BOUND`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fold {
    /// The folded items, now `released` into the fold item.
    pub folded: Vec<String>,
    /// The new `held_fold` item.
    pub item: HeldItem,
}

/// What [`HeldStore::add`] did: the id the item is held under (a new one
/// if its own collided), and the fold it caused, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Added {
    pub id: String,
    pub fold: Option<Fold>,
}

pub struct HeldStore {
    path: PathBuf,
    mode: DeliveryMode,
    lease_until_ms: Option<u64>,
    mode_ms: u64,
    entries: Vec<Entry>,
    /// Lines in the file now (0: no file yet).
    lines: usize,
    /// The file's tail may be torn or behind memory: rewrite it whole next.
    dirty: bool,
    /// The file exists but couldn't be read: never rewrite it.
    unreadable: bool,
}

#[derive(Serialize)]
struct ModeLine {
    v: u64,
    op: &'static str,
    mode: DeliveryMode,
    lease_until_ms: Option<u64>,
    ts_ms: u64,
}

#[derive(Serialize)]
struct ItemRef<'a> {
    #[serde(flatten)]
    item: &'a HeldItem,
    text: &'a str,
}

#[derive(Serialize)]
struct ItemLine<'a> {
    v: u64,
    op: &'static str,
    item: ItemRef<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<&'a Event>,
}

#[derive(Serialize)]
struct StateLine<'a> {
    v: u64,
    op: &'static str,
    ids: &'a [String],
    state: HeldState,
    side_session: Option<&'a str>,
    ts_ms: u64,
}

#[derive(Deserialize)]
struct ModeIn {
    mode: DeliveryMode,
    #[serde(default)]
    lease_until_ms: Option<u64>,
    #[serde(default)]
    ts_ms: u64,
}

#[derive(Deserialize)]
struct ItemIn {
    item: HeldItemWithText,
    #[serde(default)]
    event: Option<Event>,
}

#[derive(Deserialize)]
struct StateIn {
    ids: Vec<String>,
    state: HeldState,
    #[serde(default)]
    side_session: Option<String>,
    #[serde(default)]
    ts_ms: u64,
}

fn mode_line(mode: DeliveryMode, lease_until_ms: Option<u64>, ts_ms: u64) -> String {
    serde_json::to_string(&ModeLine { v: STORE_VERSION, op: "mode", mode, lease_until_ms, ts_ms })
        .expect("a mode line serializes")
}

fn item_line(e: &Entry) -> String {
    serde_json::to_string(&ItemLine {
        v: STORE_VERSION,
        op: "item",
        item: ItemRef { item: &e.item, text: &e.text },
        event: e.event.as_ref(),
    })
    .expect("an item line serializes")
}

fn state_line(ids: &[String], state: HeldState, side_session: Option<&str>, ts_ms: u64) -> String {
    serde_json::to_string(&StateLine { v: STORE_VERSION, op: "state", ids, state, side_session, ts_ms })
        .expect("a state line serializes")
}

/// The held item for `event`, and its full text. `headline`: the first
/// non-blank line of what the model would see of the text (a class the
/// disclosure gate withholds gives its marker, never the text: a digest
/// puts headlines into the context), control characters as spaces,
/// sanitized like the envelope's text, at most [`HEADLINE_MAX_CHARS`].
pub fn item_for_event(event: &Event, own_subagent: bool, ts_ms: u64) -> (HeldItem, String) {
    use agent_core::disclosure::{gate_for_model, DisclosureClass, ModelVisibility};
    let class = event.content.disclosure.unwrap_or(DisclosureClass::ModelVisible);
    let visible = match gate_for_model(class, &event.content.text, false, None) {
        ModelVisibility::Visible(text) => text,
        ModelVisibility::Withheld(reason) => format!("[content withheld: {} ({reason})]", class.as_str()),
    };
    let item = HeldItem {
        id: new_held_id(),
        ts_ms,
        kind: event.content.content_type.clone(),
        source: event.source.source_type.clone(),
        severity: HeldSeverity::of(event.content.severity.as_ref()),
        own_subagent,
        headline: headline(&visible),
        state: HeldState::Held,
        side_session: None,
    };
    (item, event.content.text.clone())
}

/// See [`item_for_event`].
pub fn headline(text: &str) -> String {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let plain: String = first.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let safe = crate::events::format::sanitize_text(&plain);
    let safe = safe.trim();
    if safe.chars().count() <= HEADLINE_MAX_CHARS {
        safe.to_string()
    } else {
        let mut cut: String = safe.chars().take(HEADLINE_MAX_CHARS - 1).collect();
        cut.push('…');
        cut
    }
}

/// Create/append with mode 0600 (the file is the daemon's own).
fn open_append(path: &Path) -> io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

impl HeldStore {
    /// `<base>/events/held`.
    pub fn dir_under(base: &Path) -> PathBuf {
        base.join("events").join("held")
    }

    /// `<dir>/<session id>.jsonl`, the id made filename-safe.
    pub fn path_in(dir: &Path, session_id: &str) -> PathBuf {
        dir.join(format!("{}.jsonl", crate::events::registry::sanitize_session_id(session_id)))
    }

    /// An empty `auto` store at `path`, the file not read yet: the actor
    /// builds this and reads the file ([`HeldStore::open`]) when it starts.
    pub fn unopened(path: PathBuf) -> Self {
        Self {
            path,
            mode: DeliveryMode::Auto,
            lease_until_ms: None,
            mode_ms: 0,
            entries: Vec::new(),
            lines: 0,
            dirty: false,
            unreadable: false,
        }
    }

    /// The store at `path`, folded from the file (a missing file is an
    /// empty `auto` store, and no file is made until the first change).
    /// Prunes released/dismissed items older than [`PRUNE_AFTER_MS`].
    pub fn open(path: PathBuf, now_ms: u64) -> Self {
        let mut store = Self {
            path,
            mode: DeliveryMode::Auto,
            lease_until_ms: None,
            mode_ms: 0,
            entries: Vec::new(),
            lines: 0,
            dirty: false,
            unreadable: false,
        };
        let mut raw = String::new();
        match std::fs::File::open(&store.path).and_then(|mut f| f.read_to_string(&mut raw)) {
            Ok(_) => store.fold(&raw),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                store.unreadable = true;
                tracing::warn!(path = %store.path.display(), error = %e, "held store: unreadable; it is appended to, never rewritten");
            }
        }
        let before = store.entries.len();
        store.entries.retain(|e| {
            !(matches!(e.item.state, HeldState::Released | HeldState::Dismissed)
                && now_ms.saturating_sub(e.changed_ms) >= PRUNE_AFTER_MS)
        });
        if store.entries.len() != before {
            store.dirty = true;
        }
        if store.dirty || store.lines > store.compact_threshold() {
            store.compact(now_ms);
        }
        store
    }

    fn fold(&mut self, raw: &str) {
        let torn_tail = !raw.is_empty() && !raw.ends_with('\n');
        let mut skipped = 0usize;
        for line in raw.lines() {
            self.lines += 1;
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                skipped += 1;
                continue;
            };
            match v.get("v").and_then(serde_json::Value::as_u64) {
                Some(n) if n <= STORE_VERSION => {}
                _ => {
                    skipped += 1;
                    continue;
                }
            }
            let ok = match v.get("op").and_then(serde_json::Value::as_str) {
                Some("mode") => serde_json::from_value::<ModeIn>(v).map(|m| {
                    self.mode = m.mode;
                    self.lease_until_ms = if m.mode == DeliveryMode::Hold { m.lease_until_ms } else { None };
                    self.mode_ms = m.ts_ms;
                }),
                Some("item") => serde_json::from_value::<ItemIn>(v).map(|i| {
                    let entry = Entry {
                        changed_ms: i.item.item.ts_ms,
                        item: i.item.item,
                        text: i.item.text,
                        event: i.event,
                    };
                    match self.entries.iter_mut().find(|e| e.item.id == entry.item.id) {
                        Some(e) => *e = entry,
                        None => self.entries.push(entry),
                    }
                }),
                Some("state") => serde_json::from_value::<StateIn>(v).map(|s| {
                    self.apply_state(&s.ids, s.state, s.side_session.as_deref(), s.ts_ms);
                }),
                _ => Ok(()),
            };
            if ok.is_err() {
                skipped += 1;
            }
        }
        if torn_tail {
            self.dirty = true;
        }
        if skipped > 0 {
            tracing::warn!(path = %self.path.display(), skipped, "held store: skipped lines it could not read");
        }
    }

    /// A state change on known ids; `side_session` is set when given and
    /// kept otherwise. Returns the ids it changed.
    fn apply_state(&mut self, ids: &[String], state: HeldState, side_session: Option<&str>, ts_ms: u64) -> Vec<String> {
        let mut changed = Vec::new();
        for e in self.entries.iter_mut().filter(|e| ids.contains(&e.item.id)) {
            e.item.state = state;
            if let Some(s) = side_session {
                e.item.side_session = Some(s.to_string());
            }
            e.changed_ms = ts_ms;
            changed.push(e.item.id.clone());
        }
        changed
    }

    fn compact_threshold(&self) -> usize {
        (2 * (2 * self.entries.len() + 1)).max(64)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn mode(&self) -> DeliveryMode {
        self.mode
    }

    /// `None` while `auto`.
    pub fn lease_until_ms(&self) -> Option<u64> {
        self.lease_until_ms
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.item.id == id)
    }

    /// Items in state held or join_next.
    pub fn pending(&self) -> usize {
        self.entries.iter().filter(|e| e.item.state.is_pending()).count()
    }

    /// `held_list`'s value.
    pub fn list(&self) -> HeldList {
        HeldList {
            mode: self.mode,
            lease_until_ms: self.lease_until_ms,
            items: self.entries.iter().filter(|e| e.item.state.is_listed()).map(|e| e.item.clone()).collect(),
        }
    }

    /// Record the mode and its lease (`None` with auto).
    pub fn set_mode(&mut self, mode: DeliveryMode, lease_until_ms: Option<u64>, now_ms: u64) {
        self.mode = mode;
        self.lease_until_ms = if mode == DeliveryMode::Hold { lease_until_ms } else { None };
        self.mode_ms = now_ms;
        let line = mode_line(self.mode, self.lease_until_ms, now_ms);
        self.write(vec![line], now_ms);
    }

    /// Hold `entry`; past [`HELD_BOUND`] pending items, fold the oldest
    /// held ones below high severity into one `held_fold` item.
    pub fn add(&mut self, mut entry: Entry, now_ms: u64) -> Added {
        while self.get(&entry.item.id).is_some() {
            entry.item.id = new_held_id();
        }
        let id = entry.item.id.clone();
        let mut lines = vec![item_line(&entry)];
        self.entries.push(entry);
        let fold = self.fold_over_bound(now_ms);
        if let Some(f) = &fold {
            let fold_entry = self.entries.last().expect("the fold item was pushed");
            lines.push(state_line(&f.folded, HeldState::Released, None, now_ms));
            lines.push(item_line(fold_entry));
        }
        self.write(lines, now_ms);
        Added { id, fold }
    }

    fn fold_over_bound(&mut self, now_ms: u64) -> Option<Fold> {
        let pending = self.pending();
        if pending <= HELD_BOUND {
            return None;
        }
        let need = pending - FOLD_LOW_WATER + 1;
        let picked: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.item.state == HeldState::Held && !e.item.severity.is_high())
            .map(|(i, _)| i)
            .take(need)
            .collect();
        if picked.len() < 2 {
            return None;
        }
        let mut text = String::new();
        let mut severity = HeldSeverity::Low;
        let mut folded = Vec::with_capacity(picked.len());
        for &i in &picked {
            let e = &mut self.entries[i];
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!(
                "- {} [{}] {}: {}",
                e.item.id,
                e.item.severity.as_str(),
                e.item.source,
                e.item.headline
            ));
            severity = severity.max(e.item.severity);
            e.item.state = HeldState::Released;
            e.changed_ms = now_ms;
            folded.push(e.item.id.clone());
        }
        let oldest = self.entries[picked[0]].item.ts_ms;
        let mut item = HeldItem {
            id: new_held_id(),
            ts_ms: oldest,
            kind: HELD_FOLD_TYPE.to_string(),
            source: "synaps".to_string(),
            severity,
            own_subagent: false,
            headline: format!("{} older events folded", folded.len()),
            state: HeldState::Held,
            side_session: None,
        };
        while self.get(&item.id).is_some() {
            item.id = new_held_id();
        }
        // A held item's clock is its arrival, as a reader of the file sees it.
        self.entries.push(Entry { item: item.clone(), text, event: None, changed_ms: oldest });
        Some(Fold { folded, item })
    }

    /// Set `state` on the known ones of `ids` (`side_session`: set when
    /// given). Returns the ids it changed, in store order.
    pub fn set_state(
        &mut self,
        ids: &[String],
        state: HeldState,
        side_session: Option<&str>,
        now_ms: u64,
    ) -> Vec<String> {
        let changed = self.apply_state(ids, state, side_session, now_ms);
        if !changed.is_empty() {
            let line = state_line(&changed, state, side_session, now_ms);
            self.write(vec![line], now_ms);
        }
        changed
    }

    fn write(&mut self, lines: Vec<String>, now_ms: u64) {
        if self.dirty || self.lines + lines.len() > self.compact_threshold() {
            self.compact(now_ms);
            return;
        }
        let mut buf = String::new();
        for l in &lines {
            buf.push_str(l);
            buf.push('\n');
        }
        let res = (|| {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            open_append(&self.path)?.write_all(buf.as_bytes())
        })();
        match res {
            Ok(()) => self.lines += lines.len(),
            Err(e) => {
                self.dirty = true;
                tracing::warn!(path = %self.path.display(), error = %e, "held store: append failed; the next change rewrites the file");
            }
        }
    }

    /// Rewrite the file from memory: a temp file in the same dir, synced,
    /// renamed over it. Never over a file this image couldn't read.
    pub fn compact(&mut self, now_ms: u64) {
        if self.unreadable {
            return;
        }
        let mut buf = mode_line(self.mode, self.lease_until_ms, if self.mode_ms == 0 { now_ms } else { self.mode_ms });
        buf.push('\n');
        let mut lines = 1;
        for e in &self.entries {
            buf.push_str(&item_line(e));
            buf.push('\n');
            lines += 1;
            if e.item.state != HeldState::Held {
                buf.push_str(&state_line(
                    std::slice::from_ref(&e.item.id),
                    e.item.state,
                    e.item.side_session.as_deref(),
                    e.changed_ms,
                ));
                buf.push('\n');
                lines += 1;
            }
        }
        match self.write_replacing(buf.as_bytes()) {
            Ok(()) => {
                self.lines = lines;
                self.dirty = false;
            }
            Err(e) => {
                self.dirty = true;
                tracing::warn!(path = %self.path.display(), error = %e, "held store: rewrite failed; kept in memory, tried again at the next change");
            }
        }
    }

    fn write_replacing(&self, bytes: &[u8]) -> io::Result<()> {
        let dir = self.path.parent().ok_or_else(|| io::Error::other("held store path has no parent"))?;
        std::fs::create_dir_all(dir)?;
        let name = self.path.file_name().and_then(|n| n.to_str()).unwrap_or("held");
        let tmp = dir.join(format!(".{name}.tmp-{}-{:08x}", std::process::id(), rand::random::<u32>()));
        let res = (|| {
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600);
            }
            let mut f = o.open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{EventChannel, Severity};

    const NOW: u64 = 1_761_026_400_000;
    const DAY: u64 = 24 * 60 * 60 * 1000;

    fn event(text: &str, severity: Severity) -> Event {
        let mut e = Event::simple("master", text, Some(severity));
        e.channel = Some(EventChannel { id: "c1".into(), name: "fleet".into() });
        e.content.data = Some(serde_json::json!({ "k": 1 }));
        e
    }

    fn entry(text: &str, severity: Severity, ts: u64) -> Entry {
        let ev = event(text, severity);
        let (item, text) = item_for_event(&ev, false, ts);
        Entry { item, text, event: Some(ev), changed_ms: ts }
    }

    fn store(dir: &tempfile::TempDir) -> HeldStore {
        HeldStore::open(HeldStore::path_in(&HeldStore::dir_under(dir.path()), "20261021-013000-ab12"), NOW)
    }

    fn read(s: &HeldStore) -> String {
        std::fs::read_to_string(s.path()).unwrap()
    }

    fn same(a: &HeldStore, b: &HeldStore) {
        assert_eq!(a.mode(), b.mode());
        assert_eq!(a.lease_until_ms(), b.lease_until_ms());
        assert_eq!(a.entries().len(), b.entries().len());
        for (x, y) in a.entries().iter().zip(b.entries()) {
            assert_eq!(x.item, y.item);
            assert_eq!(x.text, y.text);
            assert_eq!(x.changed_ms, y.changed_ms);
            assert_eq!(
                x.event.as_ref().map(|e| serde_json::to_value(e).unwrap()),
                y.event.as_ref().map(|e| serde_json::to_value(e).unwrap())
            );
        }
    }

    #[test]
    fn path_is_under_events_held_and_filename_safe() {
        let p = HeldStore::path_in(&HeldStore::dir_under(Path::new("/b")), "../x/y");
        assert_eq!(p, Path::new("/b/events/held/___x_y.jsonl"));
    }

    #[test]
    fn no_file_until_the_first_change() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = store(&dir);
        assert_eq!(s.mode(), DeliveryMode::Auto);
        assert!(!s.path().exists());
    }

    /// The lines are the contract's, key order included.
    #[test]
    fn lines_are_the_contracts() {
        assert_eq!(
            mode_line(DeliveryMode::Hold, Some(1761027000000), 7),
            r#"{"v":1,"op":"mode","mode":"hold","lease_until_ms":1761027000000,"ts_ms":7}"#
        );
        assert_eq!(
            state_line(&["h-0123456789abcdef".to_string()], HeldState::Dismissed, None, 9),
            r#"{"v":1,"op":"state","ids":["h-0123456789abcdef"],"state":"dismissed","side_session":null,"ts_ms":9}"#
        );
        let e = entry("hello\nsecond", Severity::High, NOW);
        // Key order, read off the line itself (a parsed Value sorts keys).
        let line = item_line(&e);
        assert!(line.starts_with(r#"{"v":1,"op":"item","item":{"id":""#), "{line}");
        let keys = [
            r#""id":"#,
            r#""ts_ms":"#,
            r#""type":"#,
            r#""source":"#,
            r#""severity":"#,
            r#""own_subagent":"#,
            r#""headline":"#,
            r#""state":"#,
            r#""side_session":"#,
            r#""text":"#,
            r#"},"event":{"#,
        ];
        let at: Vec<usize> = keys
            .iter()
            .map(|k| line.find(k).unwrap_or_else(|| panic!("{k} missing: {line}")))
            .collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "key order: {line}");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 4, "v, op, item, event: {line}");
        assert_eq!(v["item"].as_object().unwrap().len(), 10, "HeldItem + text: {line}");
        assert_eq!(v["item"]["text"], "hello\nsecond");
        assert_eq!(v["item"]["headline"], "hello");
        assert_eq!(v["item"]["severity"], "high");
        assert_eq!(v["event"], serde_json::to_value(e.event.as_ref().unwrap()).unwrap());
    }

    /// Mode, items (with their events) and states survive a reopen.
    #[test]
    fn round_trip_through_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        s.set_mode(DeliveryMode::Hold, Some(NOW + 300_000), NOW);
        let a = entry("a", Severity::Low, NOW + 1);
        let b = entry("b", Severity::Critical, NOW + 2);
        let c = entry("c", Severity::Medium, NOW + 3);
        let (ida, idb) = (a.item.id.clone(), b.item.id.clone());
        assert!(s.add(a, NOW + 1).fold.is_none());
        s.add(b, NOW + 2);
        s.add(c, NOW + 3);
        assert_eq!(s.set_state(std::slice::from_ref(&ida), HeldState::Dismissed, None, NOW + 4), vec![ida.clone()]);
        s.set_state(std::slice::from_ref(&idb), HeldState::HandledOnSide, Some("20261021-013000-cd34"), NOW + 5);
        assert!(s.set_state(&["h-ffffffffffffffff".into()], HeldState::Released, None, NOW + 6).is_empty());
        let back = store(&dir);
        same(&s, &back);
        assert_eq!(back.mode(), DeliveryMode::Hold);
        assert_eq!(back.lease_until_ms(), Some(NOW + 300_000));
        assert_eq!(back.get(&idb).unwrap().item.side_session.as_deref(), Some("20261021-013000-cd34"));
        let list = back.list();
        assert_eq!(list.items.len(), 2, "dismissed left out of held_list");
        assert!(list.items.iter().all(|i| i.id != ida));
        // Auto drops the lease.
        let mut back = back;
        back.set_mode(DeliveryMode::Auto, Some(NOW), NOW + 7);
        assert_eq!(store(&dir).lease_until_ms(), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_0600_also_after_a_rewrite() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        s.add(entry("a", Severity::Low, NOW), NOW);
        let mode = |s: &HeldStore| std::fs::metadata(s.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&s), 0o600);
        s.compact(NOW);
        assert_eq!(mode(&s), 0o600);
    }

    /// Unknown ops, newer versions and lines that don't parse are skipped;
    /// a torn tail is rewritten before the next append lands after it.
    #[test]
    fn reader_skips_what_it_cannot_read_and_heals_a_torn_tail() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let a = entry("kept", Severity::Low, NOW);
        let ida = a.item.id.clone();
        s.add(a, NOW);
        let mut raw = read(&s);
        raw.push_str(r#"{"v":1,"op":"future_thing","x":1}"#);
        raw.push('\n');
        raw.push_str(r#"{"v":2,"op":"mode","mode":"hold","lease_until_ms":1,"ts_ms":1}"#);
        raw.push('\n');
        raw.push_str("not json\n");
        raw.push_str(r#"{"v":1,"op":"state","ids":["#); // torn
        std::fs::write(s.path(), raw).unwrap();

        let mut back = store(&dir);
        assert_eq!(back.mode(), DeliveryMode::Auto, "a v2 line is skipped");
        assert_eq!(back.entries().len(), 1);
        assert_eq!(back.get(&ida).unwrap().item.state, HeldState::Held);
        // Opening rewrote it: whole lines only.
        assert!(read(&back).ends_with('\n'));
        back.add(entry("next", Severity::Low, NOW + 1), NOW + 1);
        assert_eq!(store(&dir).entries().len(), 2);
    }

    /// Growth past the content rewrites the file (temp + rename, nothing
    /// left behind); the compacted file starts with the mode line and
    /// reads back the same.
    #[test]
    fn compaction_rewrites_in_place() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        s.set_mode(DeliveryMode::Hold, Some(NOW + 60_000), NOW);
        let a = entry("a", Severity::Low, NOW);
        let ida = a.item.id.clone();
        s.add(a, NOW);
        for i in 0..200 {
            let state = if i % 2 == 0 { HeldState::JoinNext } else { HeldState::Held };
            s.set_state(std::slice::from_ref(&ida), state, None, NOW + i);
        }
        let raw = read(&s);
        assert!(raw.lines().count() <= 64, "compacted: {} lines", raw.lines().count());
        assert!(raw.starts_with(r#"{"v":1,"op":"mode","mode":"hold""#), "{raw}");
        same(&s, &store(&dir));
        let names: Vec<String> = std::fs::read_dir(s.path().parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["20261021-013000-ab12.jsonl".to_string()], "no temp file left");
    }

    /// Released and dismissed items go 7 days after their last change;
    /// held ones and younger ones stay.
    #[test]
    fn prune_after_seven_days() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let old_dismissed = entry("old dismissed", Severity::Low, NOW);
        let young_released = entry("young released", Severity::Low, NOW);
        let old_held = entry("old held", Severity::Low, NOW);
        let ids = [&old_dismissed, &young_released, &old_held].map(|e| e.item.id.clone());
        for e in [old_dismissed, young_released, old_held] {
            s.add(e, NOW);
        }
        s.set_state(&[ids[0].clone()], HeldState::Dismissed, None, NOW);
        s.set_state(&[ids[1].clone()], HeldState::Released, None, NOW + 2 * DAY);
        let later = HeldStore::open(s.path().to_path_buf(), NOW + 7 * DAY + 1);
        assert!(later.get(&ids[0]).is_none(), "pruned");
        assert!(later.get(&ids[1]).is_some(), "5 days after its release");
        assert!(later.get(&ids[2]).is_some(), "held items are never pruned");
        // The prune is on disk too, and the clock of the young one survived the rewrite.
        let again = HeldStore::open(s.path().to_path_buf(), NOW + 8 * DAY + 1);
        assert!(again.get(&ids[0]).is_none());
        assert!(again.get(&ids[1]).is_some());
        assert!(HeldStore::open(s.path().to_path_buf(), NOW + 9 * DAY + 1).get(&ids[1]).is_none());
    }

    /// GUARD ROW (the 1000 bound): past it the OLDEST held items below high
    /// severity fold into ONE held_fold item; high/critical items are
    /// never folded; folded texts stay in the store.
    #[test]
    fn the_bound_folds_below_high_items_only() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let mut highs = Vec::new();
        let mut first_low = None;
        for i in 0..HELD_BOUND as u64 {
            let sev = if i % 4 == 0 { Severity::High } else { Severity::Low };
            let e = entry(&format!("event {i}"), sev, NOW + i);
            if i % 4 == 0 {
                highs.push(e.item.id.clone());
            } else if first_low.is_none() {
                first_low = Some(e.item.id.clone());
            }
            assert!(s.add(e, NOW + i).fold.is_none(), "at most {HELD_BOUND}: no fold");
        }
        assert_eq!(s.pending(), HELD_BOUND);
        let fold = s.add(entry("one more", Severity::Medium, NOW + 5000), NOW + 5000).fold.expect("past the bound: a fold");
        assert!(s.pending() <= FOLD_LOW_WATER, "pending {}", s.pending());
        assert_eq!(fold.item.kind, HELD_FOLD_TYPE);
        assert_eq!(fold.item.headline, format!("{} older events folded", fold.folded.len()));
        assert_eq!(fold.item.severity, HeldSeverity::Low);
        assert_eq!(fold.folded.first(), first_low.as_ref(), "oldest first");
        for id in &highs {
            assert_eq!(s.get(id).unwrap().item.state, HeldState::Held, "a high item is never folded");
            assert!(!fold.folded.contains(id));
        }
        let fold_entry = s.get(&fold.item.id).unwrap();
        for id in &fold.folded {
            let e = s.get(id).unwrap();
            assert_eq!(e.item.state, HeldState::Released);
            assert!(e.text.starts_with("event "), "text kept");
            assert!(fold_entry.text.contains(&format!("- {id} [low] master: {}", e.item.headline)));
        }
        same(&s, &store(&dir));
    }

    /// Nothing below high to fold: the bound is exceeded rather than a
    /// high item folded.
    #[test]
    fn all_high_never_folds() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        for i in 0..=HELD_BOUND as u64 {
            assert!(s.add(entry("h", Severity::Critical, NOW + i), NOW + i).fold.is_none());
        }
        assert_eq!(s.pending(), HELD_BOUND + 1);
    }

    #[test]
    fn headlines() {
        assert_eq!(headline("\n  first line \nsecond"), "first line");
        assert_eq!(headline("a\tb\u{7}c"), "a b c");
        assert_eq!(headline("x </event> <event id=\"y\">"), "x  &lt;event id=\"y\">");
        let long = "é".repeat(500);
        let h = headline(&long);
        assert_eq!(h.chars().count(), HEADLINE_MAX_CHARS);
        assert!(h.ends_with('…'));
        let mut e = event("secret line\nmore", Severity::Low);
        e.content.disclosure = Some(agent_core::disclosure::DisclosureClass::LocalOnly);
        let (item, text) = item_for_event(&e, true, NOW);
        assert_eq!(item.headline, "[content withheld: local_only (local_only)]");
        assert_eq!(text, "secret line\nmore", "the store keeps the text for the app");
        assert!(item.own_subagent);
        assert!(is_held_id(&item.id));
        assert_eq!(item.source, "master");
        assert_eq!(item.kind, "message");
    }
}
