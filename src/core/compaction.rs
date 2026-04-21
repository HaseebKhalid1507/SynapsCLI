//! Conversation compaction — turn a long message history into a structured summary.

use serde_json::{json, Value};

/// System prompt used for the compaction API call.
/// Instructs the model to summarize, not continue the conversation.
pub const COMPACTION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI coding assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

use crate::runtime::Runtime;
use crate::error::Result;


const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

const UPDATE_SUMMARIZATION_PROMPT: &str = r#"The messages above are NEW conversation messages to incorporate into the existing summary provided earlier in the conversation.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

struct FileOps {
    read: std::collections::HashSet<String>,
    written: std::collections::HashSet<String>,
    edited: std::collections::HashSet<String>,
}

impl FileOps {
    fn new() -> Self {
        Self {
            read: std::collections::HashSet::new(),
            written: std::collections::HashSet::new(),
            edited: std::collections::HashSet::new(),
        }
    }
}

/// Serialize the in-memory API message history into a readable transcript and
/// ask the LLM to produce a structured summary. Called by `/compact`.
pub async fn compact_conversation(
    api_messages: &[Value],
    runtime: &Runtime,
    custom_instructions: Option<&str>,
) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut file_ops = FileOps::new();

    for msg in api_messages {
        match msg["role"].as_str() {
            Some("user") => {
                if let Some(content) = msg["content"].as_str() {
                    if content.contains("<context-summary>") {
                        parts.push(format!("[Previous Summary]: {}", content));
                    } else {
                        parts.push(format!("[User]: {}", content));
                    }
                } else if let Some(content) = msg["content"].as_array() {
                    // Tool results are shaped as user messages with tool_result blocks.
                    for block in content {
                        if block["type"].as_str() == Some("tool_result") {
                            let id = block["tool_use_id"].as_str().unwrap_or("?");
                            let text = block["content"].as_str()
                                .or_else(|| block["content"].as_array()
                                    .and_then(|a| a.first())
                                    .and_then(|b| b["text"].as_str()))
                                .unwrap_or("");
                            let truncated: String = text.chars().take(2000).collect();
                            if !truncated.is_empty() {
                                parts.push(format!("[Tool result #{}]: {}", id, truncated));
                            }
                        }
                    }
                }
            }
            Some("assistant") => {
                if let Some(content) = msg["content"].as_array() {
                    for block in content {
                        match block["type"].as_str() {
                            Some("thinking") => {
                                if let Some(text) = block["thinking"].as_str() {
                                    let preview: String = text.chars().take(500).collect();
                                    parts.push(format!("[Assistant thinking]: {}", preview));
                                }
                            }
                            Some("text") => {
                                if let Some(text) = block["text"].as_str() {
                                    parts.push(format!("[Assistant]: {}", text));
                                }
                            }
                            Some("tool_use") => {
                                let id = block["id"].as_str().unwrap_or("?");
                                let name = block["name"].as_str().unwrap_or("");
                                let input = &block["input"];
                                if let Some(path) = input["path"].as_str() {
                                    match name {
                                        "read" => { file_ops.read.insert(path.to_string()); }
                                        "write" => { file_ops.written.insert(path.to_string()); }
                                        "edit" => { file_ops.edited.insert(path.to_string()); }
                                        _ => {}
                                    }
                                }
                                let args_str = serde_json::to_string(input).unwrap_or_default();
                                let truncated: String = args_str.chars().take(500).collect();
                                parts.push(format!("[Tool call #{}: {}({})]", id, name, truncated));
                            }
                            _ => {}
                        }
                    }
                } else if let Some(content) = msg["content"].as_str() {
                    parts.push(format!("[Assistant]: {}", content));
                }
            }
            _ => {}
        }
    }

    let conversation_text = parts.join("\n\n");

    // Build file-operations summary (read-only = read but not modified).
    let modified: std::collections::HashSet<String> =
        file_ops.written.union(&file_ops.edited).cloned().collect();
    let read_only: Vec<String> = file_ops.read.difference(&modified).cloned().collect();
    let modified_list: Vec<String> = modified.into_iter().collect();

    let mut file_section = String::new();
    if !read_only.is_empty() {
        file_section.push_str(&format!(
            "\n\n<read-files>\n{}\n</read-files>",
            read_only.join("\n")
        ));
    }
    if !modified_list.is_empty() {
        file_section.push_str(&format!(
            "\n\n<modified-files>\n{}\n</modified-files>",
            modified_list.join("\n")
        ));
    }

    // Iterative compaction — if the first user message already contains a
    // summary wrapper, we're compacting on top of a previous compaction.
    let has_previous_summary = api_messages.first()
        .and_then(|m| m["content"].as_str())
        .is_some_and(|c| c.contains("<context-summary>"));

    let base_prompt = if has_previous_summary {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    };

    let mut prompt_text = format!("<conversation>\n{}\n</conversation>\n\n", conversation_text);
    if let Some(instructions) = custom_instructions {
        prompt_text.push_str(&format!("{}\n\nAdditional focus: {}", base_prompt, instructions));
    } else {
        prompt_text.push_str(base_prompt);
    }
    prompt_text.push_str(&format!(
        "\n\nAlso append these file operation records to the end of your summary:{}",
        file_section
    ));

    let user_msg = json!({"role": "user", "content": prompt_text});
    runtime.compact_call(vec![user_msg]).await
}
