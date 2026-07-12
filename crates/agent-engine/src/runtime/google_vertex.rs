//! Google Vertex public catalog and generateContent wire adapter.

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn catalog_path_and_filter_are_pinned() {
        let context = VertexRuntimeContext::new("my-project-123", "us-central1").unwrap();
        assert_eq!(catalog_path(&context, None).unwrap(), "/v1/projects/my-project-123/locations/us-central1/publishers/google/models");
        let page = br#"{"publisherModels":[{"name":"publishers/google/models/gemini-2.0-flash","displayName":"Flash","supportedActions":{"streamGenerateContent":true}},{"name":"publishers/acme/models/evil","supportedActions":{"streamGenerateContent":true}},{"name":"publishers/google/models/embed","supportedActions":{"predict":true}}],"nextPageToken":"next"}"#;
        let parsed = parse_catalog_page(page).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].id, "google-vertex/publishers/google/models/gemini-2.0-flash");
        assert_eq!(parsed.next_page_token.as_deref(), Some("next"));
    }
    #[test] fn rejects_page_token_injection_and_wrong_hosts_models() {
        let c = VertexRuntimeContext::new("my-project-123", "us-central1").unwrap();
        assert!(catalog_path(&c, Some("a/b")).is_err());
        assert!(validate_runtime_url("https://us-central1-aiplatform.googleapis.com/v1/projects/my-project-123/locations/us-central1/publishers/google/models/gemini:streamGenerateContent?alt=sse", &c).is_ok());
        assert!(validate_runtime_url("https://us-central1-aiplatform.googleapis.com.evil/x", &c).is_err());
        assert!(runtime_path(&c, "google-vertex/publishers/acme/models/x", true).is_err());
    }
    #[test] fn public_request_has_no_code_assist_envelope() {
        let c = VertexRuntimeContext::new("my-project-123", "us-central1").unwrap();
        let path = runtime_path(&c, "google-vertex/publishers/google/models/gemini-2.0-flash", true).unwrap();
        assert!(path.ends_with(":streamGenerateContent?alt=sse"));
        let body = build_request(&[Message::User("hello".into())], &[Tool { name: "weather".into(), description: "Weather".into(), parameters: serde_json::json!({"type":"object"}) }]);
        assert_eq!(body["contents"][0]["parts"][0]["text"], "hello");
        assert!(body.get("request").is_none()); assert!(body.get("project").is_none()); assert!(body.get("model").is_none());
    }
    #[test] fn sse_handles_fragmentation_tools_usage_and_eof_without_done() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hel").unwrap().is_empty());
        let events = decoder.push(b"lo\"},{\"functionCall\":{\"name\":\"weather\",\"args\":{\"city\":\"x\"}}}],\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":2,\"candidatesTokenCount\":3,\"totalTokenCount\":5}}\n\n").unwrap();
        assert!(events.iter().any(|e| matches!(e, VertexEvent::Text(t) if t == "hello")));
        assert!(events.iter().any(|e| matches!(e, VertexEvent::ToolCall { name, .. } if name == "weather")));
        assert!(events.iter().any(|e| matches!(e, VertexEvent::Usage { total: 5, .. })));
        assert!(events.iter().any(|e| matches!(e, VertexEvent::Finish(Some(r)) if r == "STOP")));
        assert!(decoder.finish().unwrap().is_empty());
    }
}
