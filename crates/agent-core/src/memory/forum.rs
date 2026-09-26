//! Pure project-forum contract, shared by host and independently built Axel service.
//! Digests are content identity, not authentication. Peer text is lower-authority data.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_BODY: usize = 8192;
pub const MAX_TITLE: usize = 256;
pub const MAX_QUERY: usize = 512;
pub const MAX_LIMIT: usize = 16;
pub const PAGE_BYTES: usize = 24 * 1024;
pub const PAGE_BODY_BYTES: usize = 16 * 1024;
pub const SNIPPET_BYTES: usize = 400;
pub const NAMESPACE: &str = "forum";
pub const META_KEY: &str = "_synaps_forum";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Author {
    pub actor: String,
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}
impl Author {
    pub fn fresh() -> Self {
        Self {
            actor: format!("actor-{}", uuid::Uuid::new_v4().simple()),
            group: format!("group-{}", uuid::Uuid::new_v4().simple()),
            parent: None,
        }
    }
    pub fn child(&self) -> Self {
        Self {
            actor: format!("actor-{}", uuid::Uuid::new_v4().simple()),
            group: self.group.clone(),
            parent: Some(self.actor.clone()),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if !prefixed_hex(&self.actor, "actor-", 32)
            || !prefixed_hex(&self.group, "group-", 32)
            || self
                .parent
                .as_ref()
                .is_some_and(|p| !prefixed_hex(p, "actor-", 32) || p == &self.actor)
        {
            return Err("invalid forum author".into());
        }
        Ok(())
    }
}
impl Default for Author {
    fn default() -> Self {
        Self::fresh()
    }
}

pub fn is_digest(s: &str) -> bool {
    hex(s, 64)
}
fn hex(s: &str, length: usize) -> bool {
    s.len() == length
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn prefixed_hex(s: &str, prefix: &str, length: usize) -> bool {
    s.strip_prefix(prefix).is_some_and(|tail| hex(tail, length))
}
pub fn valid_id(s: &str) -> bool {
    prefixed_hex(s, "msg-", 64)
}
pub fn valid_project(s: &str) -> bool {
    prefixed_hex(s, "p", 16) && s != "p0000000000000000"
}
fn text(s: &str, max: usize, multiline: bool) -> bool {
    s.len() <= max
        && !s
            .chars()
            .any(|c| c.is_control() && !(multiline && matches!(c, '\n' | '\t')))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Post {
    pub request_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub title: String,
    pub body: String,
    pub retention_days: u32,
}
impl Post {
    pub fn validate(&self) -> Result<(), String> {
        if self.request_key.is_empty()
            || self.request_key.len() > 64
            || !self
                .request_key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err("forum request_key must be 1..64 ASCII letters/digits/._-".into());
        }
        if !(1..=365).contains(&self.retention_days)
            || !text(&self.body, MAX_BODY, true)
            || self.body.trim().is_empty()
            || !text(&self.title, MAX_TITLE, false)
        {
            return Err("forum post exceeds text or retention bounds".into());
        }
        match &self.thread_id {
            None if self.title.trim().is_empty() || self.reply_to.is_some() => {
                return Err("forum root requires a title and no reply_to".into())
            }
            Some(id) if !valid_id(id) || !self.title.is_empty() => {
                return Err("forum reply requires an exact thread ID and empty title".into())
            }
            _ => {}
        }
        if self.reply_to.as_ref().is_some_and(|id| !valid_id(id)) {
            return Err("invalid forum reply_to ID".into());
        }
        Ok(())
    }
}
fn hash_parts(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for part in parts {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part);
    }
    format!("{:x}", h.finalize())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub version: u32,
    pub project: String,
    pub author: Author,
    pub request_key: String,
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub title: String,
    pub retention_days: u32,
    pub digest: String,
}
impl Envelope {
    pub fn new(project: &str, author: Author, post: &Post) -> Result<Self, String> {
        post.validate()?;
        author.validate()?;
        if !valid_project(project) {
            return Err("invalid forum repository scope".into());
        }
        let digest = hash_parts(&[
            b"synaps-forum-post/1",
            project.as_bytes(),
            author.actor.as_bytes(),
            author.group.as_bytes(),
            author.parent.as_deref().unwrap_or("").as_bytes(),
            post.request_key.as_bytes(),
            post.thread_id.as_deref().unwrap_or("").as_bytes(),
            post.reply_to.as_deref().unwrap_or("").as_bytes(),
            post.title.as_bytes(),
            post.body.as_bytes(),
            &post.retention_days.to_be_bytes(),
        ]);
        Ok(Self {
            version: 1,
            project: project.into(),
            author,
            request_key: post.request_key.clone(),
            thread_id: post
                .thread_id
                .clone()
                .unwrap_or_else(|| format!("msg-{digest}")),
            reply_to: post.reply_to.clone(),
            title: post.title.clone(),
            retention_days: post.retention_days,
            digest,
        })
    }
    pub fn id(&self) -> String {
        format!("msg-{}", self.digest)
    }
    pub fn is_root(&self) -> bool {
        self.thread_id == self.id()
    }
    pub fn source(&self) -> String {
        format!("forum:{}", self.id())
    }
    pub fn validate_metadata(&self) -> Result<(), String> {
        if self.version != 1
            || !valid_project(&self.project)
            || !is_digest(&self.digest)
            || !valid_id(&self.thread_id)
        {
            return Err("invalid forum metadata".into());
        }
        self.author.validate()?;
        let p = self.post("validation body".into());
        p.validate()
    }
    pub fn post(&self, body: String) -> Post {
        Post {
            request_key: self.request_key.clone(),
            thread_id: (!self.is_root()).then(|| self.thread_id.clone()),
            reply_to: self.reply_to.clone(),
            title: self.title.clone(),
            body,
            retention_days: self.retention_days,
        }
    }
    pub fn validate(&self, body: &str) -> Result<(), String> {
        self.validate_metadata()?;
        let expected = Self::new(&self.project, self.author.clone(), &self.post(body.into()))?;
        if *self != expected {
            return Err("forum digest mismatch".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostRequest {
    pub author: Author,
    pub post: Post,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Created,
    Duplicate,
    Tombstoned,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub id: String,
    pub thread_id: String,
    pub digest: String,
    pub status: Status,
    pub timestamp_ms: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub timestamp_ms: u64,
    pub id: String,
}
impl Cursor {
    pub fn validate(&self) -> Result<(), String> {
        if self.timestamp_ms > i64::MAX as u64 || !valid_id(&self.id) {
            return Err("invalid forum cursor".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Read {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Cursor>,
    pub limit: usize,
}
impl Read {
    pub fn validate(&self) -> Result<(), String> {
        if self.limit == 0
            || self.limit > MAX_LIMIT
            || self.thread_id.as_ref().is_some_and(|s| !valid_id(s))
            || self
                .query
                .as_ref()
                .is_some_and(|s| !text(s, MAX_QUERY, false))
        {
            return Err("invalid forum read bounds".into());
        }
        if let Some(c) = &self.after {
            c.validate()?;
        }
        Ok(())
    }
}
impl Default for Read {
    fn default() -> Self {
        Self {
            thread_id: None,
            query: None,
            after: None,
            limit: 8,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub timestamp_ms: u64,
    pub envelope: Envelope,
    pub body: String,
    pub truncated: bool,
}
impl Entry {
    pub fn cursor(&self) -> Cursor {
        Cursor {
            timestamp_ms: self.timestamp_ms,
            id: self.id.clone(),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        self.cursor().validate()?;
        self.envelope.validate_metadata()?;
        if self.id != self.envelope.id() || !text(&self.body, MAX_BODY, true) {
            return Err("invalid forum entry".into());
        }
        if self.truncated {
            if self.body.len() > SNIPPET_BYTES || !self.envelope.is_root() {
                return Err("invalid forum snippet".into());
            }
        } else {
            self.envelope.validate(&self.body)?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub next: Option<Cursor>,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn post() -> Post {
        Post {
            request_key: "finding-1".into(),
            thread_id: None,
            reply_to: None,
            title: "Finding".into(),
            body: "Synthetic useful note".into(),
            retention_days: 30,
        }
    }
    fn author() -> Author {
        Author {
            actor: format!("actor-{}", "a".repeat(32)),
            group: format!("group-{}", "b".repeat(32)),
            parent: None,
        }
    }
    #[test]
    fn digest_roundtrip_and_domain_separation() {
        let p = post();
        let e = Envelope::new("p1111111111111111", author(), &p).unwrap();
        e.validate(&p.body).unwrap();
        assert_eq!(e, Envelope::new("p1111111111111111", author(), &p).unwrap());
        assert_ne!(
            e.id(),
            Envelope::new("p2222222222222222", author(), &p)
                .unwrap()
                .id()
        );
        let mut changed = p.clone();
        changed.request_key.push('2');
        assert_ne!(
            e.id(),
            Envelope::new("p1111111111111111", author(), &changed)
                .unwrap()
                .id()
        );
        assert!(e.validate("tampered note").is_err());
    }
    #[test]
    fn exact_digest_vector() {
        let e = Envelope::new("p1111111111111111", author(), &post()).unwrap();
        assert_eq!(
            e.digest,
            "79f1294388dea6ade99737365b1f0fb55e8afe447ad9c25f5c568e03476a7ccf"
        );
    }
    #[test]
    fn bounds_and_author_validation() {
        let mut p = post();
        p.body = "x".repeat(MAX_BODY + 1);
        assert!(p.validate().is_err());
        p = post();
        p.body = "hidden\u{1b}[0m".into();
        assert!(p.validate().is_err());
        p = post();
        p.retention_days = 0;
        assert!(p.validate().is_err());
        p = post();
        p.title.clear();
        assert!(p.validate().is_err());
        let a = author();
        let child = a.child();
        assert_eq!(a.group, child.group);
        assert_eq!(child.parent, Some(a.actor.clone()));
        assert_ne!(a.actor, child.actor);
        child.validate().unwrap();
    }
    #[test]
    fn replies_require_exact_thread_and_empty_title() {
        let mut p = post();
        let root = Envelope::new("p1111111111111111", author(), &p).unwrap();
        p.thread_id = Some(root.id());
        assert!(p.validate().is_err());
        p.title.clear();
        let reply = Envelope::new("p1111111111111111", author(), &p).unwrap();
        assert!(!reply.is_root());
        reply.validate(&p.body).unwrap();
        p.reply_to = Some("../../bad".into());
        assert!(p.validate().is_err());
    }
}
