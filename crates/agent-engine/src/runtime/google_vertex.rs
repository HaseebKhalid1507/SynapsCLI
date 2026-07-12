//! Provider-local Google Vertex public catalog and generateContent adapter.
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;

pub const MAX_PAGE_BYTES: usize = 1024 * 1024;
pub const MAX_SSE_BUFFER: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct VertexRuntimeContext {
    project: String,
    location: String,
}
impl VertexRuntimeContext {
    pub fn new(project: &str, location: &str) -> Result<Self, VertexRuntimeError> {
        agent_core::auth::google_vertex::VertexContext::new(project, location)
            .map_err(|_| VertexRuntimeError::InvalidContext)?;
        Ok(Self {
            project: project.into(),
            location: location.into(),
        })
    }
    fn host(&self) -> String {
        format!("{}-aiplatform.googleapis.com", self.location)
    }
}
#[derive(Debug, thiserror::Error)]
pub enum VertexRuntimeError {
    #[error("google-vertex: invalid context")]
    InvalidContext,
    #[error("google-vertex: invalid path or model")]
    InvalidPath,
    #[error("google-vertex: malformed or oversized response")]
    Malformed,
    #[error("google-vertex: provider error")]
    Provider,
}

pub fn catalog_path(
    c: &VertexRuntimeContext,
    token: Option<&str>,
) -> Result<String, VertexRuntimeError> {
    let mut p = format!(
        "/v1/projects/{}/locations/{}/publishers/google/models",
        c.project, c.location
    );
    if let Some(t) = token {
        if t.is_empty()
            || t.len() > 512
            || !t
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(VertexRuntimeError::InvalidPath);
        }
        p.push_str("?pageToken=");
        p.push_str(t);
    }
    Ok(p)
}
pub fn runtime_path(
    c: &VertexRuntimeContext,
    id: &str,
    stream: bool,
) -> Result<String, VertexRuntimeError> {
    let model = id
        .strip_prefix("google-vertex/publishers/google/models/")
        .ok_or(VertexRuntimeError::InvalidPath)?;
    if model.is_empty()
        || !model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
    {
        return Err(VertexRuntimeError::InvalidPath);
    }
    Ok(format!(
        "/v1/projects/{}/locations/{}/publishers/google/models/{}:{}{}",
        c.project,
        c.location,
        model,
        if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        },
        if stream { "?alt=sse" } else { "" }
    ))
}
pub fn validate_runtime_url(s: &str, c: &VertexRuntimeContext) -> Result<(), VertexRuntimeError> {
    let u = Url::parse(s).map_err(|_| VertexRuntimeError::InvalidPath)?;
    if u.scheme() != "https" || u.host_str() != Some(&c.host()) || u.port().is_some() {
        return Err(VertexRuntimeError::InvalidPath);
    }
    let prefix = format!(
        "/v1/projects/{}/locations/{}/publishers/google/models/",
        c.project, c.location
    );
    if !u.path().starts_with(&prefix)
        || (!u.path().ends_with(":streamGenerateContent")
            && !u.path().ends_with(":generateContent"))
    {
        return Err(VertexRuntimeError::InvalidPath);
    }
    if u.query().is_some_and(|q| q != "alt=sse") {
        return Err(VertexRuntimeError::InvalidPath);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub id: String,
    pub display_name: String,
    pub streaming: bool,
    pub tools: bool,
}
pub struct CatalogPage {
    pub entries: Vec<CatalogEntry>,
    pub next_page_token: Option<String>,
}
#[derive(Deserialize)]
struct Page {
    #[serde(rename = "publisherModels", default)]
    models: Vec<Model>,
    #[serde(rename = "nextPageToken")]
    next: Option<String>,
}
#[derive(Deserialize)]
struct Model {
    name: String,
    #[serde(rename = "displayName", default)]
    display: String,
    #[serde(rename = "supportedActions", default)]
    actions: Value,
}
pub fn parse_catalog_page(body: &[u8]) -> Result<CatalogPage, VertexRuntimeError> {
    if body.len() > MAX_PAGE_BYTES {
        return Err(VertexRuntimeError::Malformed);
    }
    let p: Page = serde_json::from_slice(body).map_err(|_| VertexRuntimeError::Malformed)?;
    if p.models.len() > 1000 {
        return Err(VertexRuntimeError::Malformed);
    }
    let mut entries = vec![];
    for m in p.models {
        if m.name.starts_with("publishers/google/models/")
            && m.actions
                .get("streamGenerateContent")
                .and_then(Value::as_bool)
                == Some(true)
        {
            entries.push(CatalogEntry {
                id: format!("google-vertex/{}", m.name),
                display_name: if m.display.is_empty() {
                    m.name
                } else {
                    m.display
                },
                streaming: true,
                tools: true,
            })
        }
    }
    Ok(CatalogPage {
        entries,
        next_page_token: p.next,
    })
}

pub enum Message {
    User(String),
    Model(String),
}
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}
pub fn build_request(messages: &[Message], tools: &[Tool]) -> Value {
    let contents: Vec<_> = messages
        .iter()
        .map(|m| match m {
            Message::User(t) => json!({"role":"user","parts":[{"text":t}]}),
            Message::Model(t) => json!({"role":"model","parts":[{"text":t}]}),
        })
        .collect();
    let declarations: Vec<_> = tools
        .iter()
        .map(|t| json!({"name":t.name,"description":t.description,"parameters":t.parameters}))
        .collect();
    let mut v = json!({"contents":contents});
    if !declarations.is_empty() {
        v["tools"] = json!([{"functionDeclarations":declarations}]);
    }
    v
}

#[derive(Debug, Clone, PartialEq)]
pub enum VertexEvent {
    Text(String),
    ToolCall {
        name: String,
        args: Value,
    },
    Usage {
        prompt: u64,
        completion: u64,
        total: u64,
    },
    Finish(Option<String>),
}
#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}
impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<VertexEvent>, VertexRuntimeError> {
        if self.buffer.len() + bytes.len() > MAX_SSE_BUFFER {
            return Err(VertexRuntimeError::Malformed);
        }
        self.buffer.extend_from_slice(bytes);
        let mut out = vec![];
        while let Some(pos) = self.buffer.windows(2).position(|w| w == b"\n\n") {
            let frame: Vec<_> = self.buffer.drain(..pos + 2).collect();
            let text = std::str::from_utf8(&frame).map_err(|_| VertexRuntimeError::Malformed)?;
            for line in text.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        continue;
                    }
                    out.extend(parse_event(data.as_bytes())?);
                }
            }
        }
        Ok(out)
    }
    pub fn finish(&mut self) -> Result<Vec<VertexEvent>, VertexRuntimeError> {
        if self.buffer.iter().all(u8::is_ascii_whitespace) {
            self.buffer.clear();
            return Ok(vec![]);
        }
        let tail = std::mem::take(&mut self.buffer);
        let s = std::str::from_utf8(&tail).map_err(|_| VertexRuntimeError::Malformed)?;
        if let Some(data) = s.trim().strip_prefix("data: ") {
            parse_event(data.as_bytes())
        } else {
            Err(VertexRuntimeError::Malformed)
        }
    }
}
fn parse_event(data: &[u8]) -> Result<Vec<VertexEvent>, VertexRuntimeError> {
    let v: Value = serde_json::from_slice(data).map_err(|_| VertexRuntimeError::Malformed)?;
    if v.get("error").is_some() {
        return Err(VertexRuntimeError::Provider);
    }
    let mut out = vec![];
    if let Some(cs) = v["candidates"].as_array() {
        for c in cs {
            if let Some(parts) = c["content"]["parts"].as_array() {
                for p in parts {
                    if let Some(t) = p["text"].as_str() {
                        out.push(VertexEvent::Text(t.into()))
                    }
                    if let Some(fc) = p.get("functionCall") {
                        out.push(VertexEvent::ToolCall {
                            name: fc["name"]
                                .as_str()
                                .ok_or(VertexRuntimeError::Malformed)?
                                .into(),
                            args: fc["args"].clone(),
                        })
                    }
                }
            }
            if let Some(r) = c["finishReason"].as_str() {
                out.push(VertexEvent::Finish(Some(r.into())))
            }
        }
    }
    if let Some(u) = v.get("usageMetadata") {
        out.push(VertexEvent::Usage {
            prompt: u["promptTokenCount"].as_u64().unwrap_or(0),
            completion: u["candidatesTokenCount"].as_u64().unwrap_or(0),
            total: u["totalTokenCount"].as_u64().unwrap_or(0),
        })
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_path_and_filter_are_pinned() {
        let c = VertexRuntimeContext::new("my-project-123", "us-central1").unwrap();
        assert_eq!(
            catalog_path(&c, None).unwrap(),
            "/v1/projects/my-project-123/locations/us-central1/publishers/google/models"
        );
        let p=parse_catalog_page(br#"{"publisherModels":[{"name":"publishers/google/models/gemini-2.0-flash","displayName":"Flash","supportedActions":{"streamGenerateContent":true}},{"name":"publishers/acme/models/evil","supportedActions":{"streamGenerateContent":true}},{"name":"publishers/google/models/embed","supportedActions":{"predict":true}}],"nextPageToken":"next"}"#).unwrap();
        assert_eq!(p.entries.len(), 1);
        assert_eq!(
            p.entries[0].id,
            "google-vertex/publishers/google/models/gemini-2.0-flash"
        );
        assert_eq!(p.next_page_token.as_deref(), Some("next"));
    }
    #[test]
    fn rejects_page_token_injection_and_wrong_hosts_models() {
        let c = VertexRuntimeContext::new("my-project-123", "us-central1").unwrap();
        assert!(catalog_path(&c, Some("a/b")).is_err());
        assert!(validate_runtime_url("https://us-central1-aiplatform.googleapis.com/v1/projects/my-project-123/locations/us-central1/publishers/google/models/gemini:streamGenerateContent?alt=sse",&c).is_ok());
        assert!(
            validate_runtime_url("https://us-central1-aiplatform.googleapis.com.evil/x", &c)
                .is_err()
        );
        assert!(runtime_path(&c, "google-vertex/publishers/acme/models/x", true).is_err());
    }
    #[test]
    fn public_request_has_no_code_assist_envelope() {
        let c = VertexRuntimeContext::new("my-project-123", "us-central1").unwrap();
        assert!(runtime_path(
            &c,
            "google-vertex/publishers/google/models/gemini-2.0-flash",
            true
        )
        .unwrap()
        .ends_with(":streamGenerateContent?alt=sse"));
        let b = build_request(
            &[Message::User("hello".into())],
            &[Tool {
                name: "weather".into(),
                description: "Weather".into(),
                parameters: json!({"type":"object"}),
            }],
        );
        assert_eq!(b["contents"][0]["parts"][0]["text"], "hello");
        assert!(b.get("request").is_none());
        assert!(b.get("project").is_none());
        assert!(b.get("model").is_none());
    }
    #[test]
    fn sse_handles_fragmentation_tools_usage_and_eof_without_done() {
        let mut d = SseDecoder::default();
        assert!(d
            .push(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hel")
            .unwrap()
            .is_empty());
        let e=d.push(b"lo\"},{\"functionCall\":{\"name\":\"weather\",\"args\":{\"city\":\"x\"}}}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":2,\"candidatesTokenCount\":3,\"totalTokenCount\":5}}\n\n").unwrap();
        assert!(e
            .iter()
            .any(|e| matches!(e,VertexEvent::Text(t)if t=="hello")));
        assert!(e
            .iter()
            .any(|e| matches!(e,VertexEvent::ToolCall{name,..}if name=="weather")));
        assert!(e
            .iter()
            .any(|e| matches!(e, VertexEvent::Usage { total: 5, .. })));
        assert!(e
            .iter()
            .any(|e| matches!(e,VertexEvent::Finish(Some(r))if r=="STOP")));
        assert!(d.finish().unwrap().is_empty());
    }
}
