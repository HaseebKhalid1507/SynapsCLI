use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EXPORT_FORMAT: &str = "synaps-axel-export/1";
pub const SCHEMA: &str = "synaps-axel/2";
// Persisted record envelopes retain their original identity across protocol upgrades.
pub const RECORD_SCHEMA: &str = "synaps-axel/1";
pub const USER_PROJECT: &str = "p0000000000000000";
pub const REVISION: &str = "edbdea401d66feedb87fcad28c879ece54e3ccd2";
pub const MAX_FRAME: usize = 1024 * 1024;
pub const MAX_LARGE_FRAME: usize = 24 * 1024 * 1024;
pub const MAX_CONTENT: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Normal,
    Sensitive,
    Secret,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    Standard,
    MaxAgeDays(u32),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MemoryRecord {
    pub namespace: String,
    pub timestamp_ms: u64,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    pub id: String,
    pub project: String,
    pub provenance: Provenance,
    pub sensitivity: Sensitivity,
    pub retention: Retention,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct MemoryDescriptor {
    pub id: String,
    pub project: String,
    pub timestamp_ms: u64,
    pub tags: Vec<String>,
    pub snippet: String,
    pub truncated: bool,
    pub content_bytes: usize,
    pub sensitivity: Sensitivity,
    pub retention: Retention,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Search {
    pub content_contains: Option<String>,
    pub tag_prefix: Option<String>,
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
    pub limit: Option<usize>,
    pub snippet_bytes: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fetch {
    pub ids: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forget {
    pub id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema: String,
    pub project: String,
    pub operation: String,
    pub payload: Value,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Scope,
    Protocol,
    TooLarge,
    NotFound,
    Conflict,
    Storage,
    CommitUnknown,
    UnsafePath,
    Unsupported,
}
impl Error {
    pub fn wire(self) -> Value {
        let (code, message) = match self {
            Self::Invalid => ("invalid_request", "Invalid request."),
            Self::Scope => ("project_mismatch", "Project scope does not match."),
            Self::Protocol => (
                "protocol_error",
                "Expected hello followed by one operation.",
            ),
            Self::TooLarge => ("size_limit", "Request or reply exceeds the size limit."),
            Self::NotFound => (
                "not_found",
                "One or more records are unavailable in this scope.",
            ),
            Self::Conflict => ("id_conflict", "Record ID is unavailable."),
            Self::Storage => ("storage_error", "Storage operation failed."),
            Self::CommitUnknown => (
                "commit_unknown",
                "Write may have committed; reconcile before retrying.",
            ),
            Self::UnsafePath => ("unsafe_path", "Brain requires a private non-symlink path."),
            Self::Unsupported => (
                "unsupported_brain",
                "Brain scope or compatibility metadata is unsupported.",
            ),
        };
        serde_json::json!({"code":code,"message":message})
    }
}
pub type Result<T> = std::result::Result<T, Error>;
impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self::Storage
    }
}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::Storage
    }
}
impl From<axel::error::AxelError> for Error {
    fn from(_: axel::error::AxelError) -> Self {
        Self::Storage
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::Invalid
    }
}

pub fn valid_project(s: &str) -> bool {
    s.len() == 17
        && s.starts_with('p')
        && s.as_bytes()[1..]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
impl MemoryRecord {
    pub fn validate(&self, project: &str) -> Result<()> {
        if self.project != project {
            return Err(Error::Scope);
        }
        let n = &self.namespace;
        if !valid_id(&self.id)
            || n.is_empty()
            || n.len() > 64
            || n.contains('/')
            || n.contains('\\')
            || n.contains("..")
            || n.chars().any(char::is_whitespace)
            || self.timestamp_ms > i64::MAX as u64
        {
            return Err(Error::Invalid);
        }
        if self.content.len() > MAX_CONTENT {
            return Err(Error::TooLarge);
        }
        chrono::DateTime::from_timestamp_millis(self.timestamp_ms as i64).ok_or(Error::Invalid)?;
        self.expires_ms()?;
        self.disclosure()?;
        crate::forum::validate_metadata(self)?;
        Ok(())
    }
    pub fn disclosure(&self) -> Result<Option<&str>> {
        let Some(meta) = self.meta.as_ref().and_then(|m| m.get("_axel")) else {
            return Ok(None);
        };
        if !meta.is_object() {
            return Err(Error::Invalid);
        }
        let Some(value) = meta.get("disclosure") else {
            return Ok(None);
        };
        match value.as_str() {
            Some(
                s
                @ ("standard" | "local_only" | "visible_after_consent" | "persist_never_transmit"),
            ) => Ok(Some(s)),
            _ => Err(Error::Invalid),
        }
    }
    pub fn expires_ms(&self) -> Result<Option<i64>> {
        let absolute = match self
            .meta
            .as_ref()
            .and_then(|m| m.get("_axel"))
            .and_then(|m| m.get("expires_ms"))
        {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_i64().filter(|n| *n >= 0).ok_or(Error::Invalid)?),
        };
        if let Some(n) = absolute {
            chrono::DateTime::from_timestamp_millis(n).ok_or(Error::Invalid)?;
        }
        let ttl = match self.retention {
            Retention::Standard => Ok::<Option<i64>, Error>(None),
            Retention::MaxAgeDays(days) => {
                let n = self
                    .timestamp_ms
                    .checked_add(u64::from(days) * 86_400_000)
                    .ok_or(Error::Invalid)?;
                let n = i64::try_from(n).map_err(|_| Error::Invalid)?;
                chrono::DateTime::from_timestamp_millis(n).ok_or(Error::Invalid)?;
                Ok(Some(n))
            }
        }?;
        Ok(match (absolute, ttl) {
            (Some(a), Some(t)) => Some(a.min(t)),
            (a, t) => a.or(t),
        })
    }
    pub fn disclosed(mut self) -> Self {
        if self.sensitivity == Sensitivity::Secret
            || self
                .disclosure()
                .ok()
                .flatten()
                .is_some_and(|d| d != "standard")
        {
            self.content.clear();
        }
        self
    }
}
