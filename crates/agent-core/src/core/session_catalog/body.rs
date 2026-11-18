//! Streaming walk over a session's message history without materialising a
//! `Session`: one `serde_json::Value` per message, everything else in the
//! snapshot skipped. Sessions with a `.journal` fall back to the canonical
//! overlay load so appended deltas are seen.

use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::io::Read;
use std::path::Path;

const STOP: &str = "\u{0}catalog-stop";

/// Read `<id>.json` through the confined, bounded session read path.
pub(crate) fn read_snapshot_bytes(dir: &Path, id: &str) -> std::io::Result<Option<Vec<u8>>> {
    let Some(file) = crate::core::session_journal::confined_open(dir, &format!("{id}.json"))?
    else {
        return Ok(None);
    };
    let cap = crate::core::session_journal::MAX_PERSISTED_READ_BYTES;
    let mut buf = Vec::with_capacity(file.metadata().map_or(0, |m| m.len() as usize));
    file.take(cap + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(std::io::Error::other("session snapshot exceeds read cap"));
    }
    Ok(Some(buf))
}

/// Call `f` for every message in order; `f` returns `false` to stop early.
/// Returns how many messages were visited and whether the walk completed.
pub(crate) fn walk_snapshot(
    bytes: &[u8],
    f: &mut dyn FnMut(&Value) -> bool,
) -> std::io::Result<(usize, bool)> {
    let mut count = 0usize;
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let res = Top {
        f,
        count: &mut count,
    }
    .deserialize(&mut de);
    match res {
        Ok(()) => Ok((count, true)),
        Err(e) if e.to_string().starts_with(STOP) => Ok((count, false)),
        Err(e) => Err(std::io::Error::other(e)),
    }
}

/// Walk one session by id, journal-aware.
pub(crate) fn walk_session(
    dir: &Path,
    id: &str,
    has_journal: bool,
    f: &mut dyn FnMut(&Value) -> bool,
) -> std::io::Result<(usize, bool)> {
    if has_journal {
        let s = crate::core::session_journal::load_session_in_dir(dir, id)?;
        let mut n = 0;
        for m in &s.api_messages {
            n += 1;
            if !f(m) {
                return Ok((n, false));
            }
        }
        return Ok((n, true));
    }
    match read_snapshot_bytes(dir, id)? {
        Some(bytes) => walk_snapshot(&bytes, f),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no session snapshot for '{id}'"),
        )),
    }
}

/// Host-injected context notes are user-role but not user prompts.
pub(crate) fn is_context_note(m: &Value) -> bool {
    m.get("_synaps_context").is_some()
}

/// User-role text the HOST injected, not something a human typed: event-bus
/// deliveries (`events::format`), abort context and interrupt markers
/// (`engine::interrupt`), and legacy inline system prompts.
pub(crate) fn is_host_injected(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("<event id=")
        || t.starts_with("[ABORT CONTEXT")
        || t.starts_with("[Request interrupted")
        || t.starts_with("<system-prompt>")
}

struct Top<'a> {
    f: &'a mut dyn FnMut(&Value) -> bool,
    count: &'a mut usize,
}

impl<'de> DeserializeSeed<'de> for Top<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Top<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a session object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let Top { f, count } = self;
        // `_hooked` precedes `api_messages` (session_journal): each message
        // gets its `before_message` count back before `f` sees it, so the
        // user's words are told apart from extension output (#414).
        let mut hooked = std::collections::HashMap::new();
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            if key == "_hooked" {
                let value: Value = map.next_value()?;
                hooked = crate::core::session_journal::stored_hooked(value).into_iter().collect();
            } else if key == "api_messages" {
                map.next_value_seed(Msgs {
                    f: &mut *f,
                    count: &mut *count,
                    hooked: &hooked,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

struct Msgs<'a> {
    f: &'a mut dyn FnMut(&Value) -> bool,
    count: &'a mut usize,
    hooked: &'a std::collections::HashMap<usize, usize>,
}

impl<'de> DeserializeSeed<'de> for Msgs<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Msgs<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an api_messages array")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(mut v) = seq.next_element::<Value>()? {
            if let Some(&n) = self.hooked.get(&*self.count) {
                crate::core::extension_context::set_hooked(&mut v, n);
            }
            *self.count += 1;
            if !(self.f)(&v) {
                return Err(de::Error::custom(STOP));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extension_context as ec;
    use crate::core::session_journal::{save_session_in_dir, SessionPersistence};

    /// #414 review: a streaming walk of a snapshot gives each message its
    /// `before_message` count back, so previews and deep search see the
    /// user's words, not extension context saved on them.
    #[test]
    fn a_snapshot_walk_restores_before_message_counts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut s = crate::core::session::Session::new("m", "low", None);
        let mut prompt = serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "go"},
            {"type": "text", "text": ec::saved_block_text("axel", "CTX")},
        ]});
        ec::set_hooked(&mut prompt, 1);
        s.api_messages.push(std::sync::Arc::new(prompt));
        save_session_in_dir(tmp.path(), &s, SessionPersistence::Json).unwrap();
        let bytes = std::fs::read(tmp.path().join(format!("{}.json", s.id))).unwrap();
        let mut seen = Vec::new();
        walk_snapshot(&bytes, &mut |m| {
            seen.push(crate::core::session::user_message_for_display(m));
            true
        })
        .unwrap();
        assert_eq!(seen, [Some("go".to_string())]);
    }
}
