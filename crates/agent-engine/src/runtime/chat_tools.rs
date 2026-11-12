//! What one chat is offered out of the host's tool registry.
//!
//! A host serves many chats (the daemon) and they share one registry, so
//! the registry cannot say what a single chat may see. A tool that is only
//! of use to some chats is registered once, for all, and withheld from the
//! session tool set of each chat that has no use for it.

use crate::tools::activation::{SessionId, SessionToolSet};
use crate::tools::catalog::{ToolCatalog, ToolId};

/// The built-in tools a chat is not offered. `context_checkpoint` writes to
/// a chat's rollover state, so a chat without context management has no use
/// for it; the project forum lives in the Axel backend, so under any other
/// binding every `forum_*` call would only fail (`require_forum`). Neither
/// is worth the catalog tokens for a chat that cannot use it.
///
/// This is decided per chat and kept in the chat's own session tool set
/// ([`chat_tool_set`]). The tools themselves stay in the registry, which
/// every chat on the host shares: removing one there removed it for all.
pub(super) fn withheld_tools(context_enabled: bool, forum_ready: bool) -> Vec<ToolId> {
    let mut names: Vec<&str> = Vec::new();
    if !context_enabled {
        names.push("context_checkpoint");
    }
    if !forum_ready {
        names.extend(["forum_post", "forum_read", "forum_forget"]);
    }
    names.into_iter().map(ToolId::builtin).collect()
}

/// The session tool set a turn of a chat starts from: the core
/// [`super::continuation::context_tool_set`] derives, without the tools the
/// chat is not offered ([`withheld_tools`]). Turn start, the keep-warm ping
/// and the frozen prompt catalog all build it here, so they agree on what
/// the chat's requests carry.
pub(super) fn chat_tool_set(
    session: SessionId,
    catalog: &ToolCatalog,
    progressive: bool,
    context_enabled: bool,
    forum_ready: bool,
    always_loaded: &[ToolId],
) -> SessionToolSet {
    super::continuation::context_tool_set(session, catalog, progressive, context_enabled, always_loaded)
        .withholding(withheld_tools(context_enabled, forum_ready))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::continuation::{ContinuationState, SharedContinuation};
    use crate::tools::activation::{activate_exact_for_user, ActivationAuthority, ExecutionGate};
    use crate::tools::discovery::ActivationCapability;
    use crate::tools::test_helpers::create_tool_context;
    use crate::tools::{Tool, ToolRegistry};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex, RwLock};

    const FORUM: [&str; 3] = ["forum_post", "forum_read", "forum_forget"];

    fn session(name: &str) -> SessionId {
        SessionId::parse(name).unwrap()
    }

    fn allowed(registry: &ToolRegistry, set: &SessionToolSet, name: &str) -> bool {
        ExecutionGate::authorize_wire_call(registry, set, name).is_ok()
    }

    fn names(schema: &[Value]) -> Vec<&str> {
        schema.iter().filter_map(|t| t["name"].as_str()).collect()
    }

    /// Another chat's doing: something registers a tool on the shared registry.
    struct Late;
    #[async_trait::async_trait]
    impl Tool for Late {
        fn name(&self) -> &str {
            "late_tool"
        }
        fn description(&self) -> &str {
            "registered while a turn runs"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn origin(&self) -> crate::tools::ToolOrigin {
            crate::tools::ToolOrigin::Builtin
        }
        async fn execute(&self, _: Value, _: crate::ToolContext) -> crate::Result<String> {
            Ok(String::new())
        }
    }

    /// One registry, two chats, whole-registry requests (progressive
    /// disclosure off). The chat with context management and a forum may
    /// call the four tools and is sent them; the chat with neither is
    /// refused each and sent none. Nothing was taken out of the registry.
    #[test]
    fn two_chats_on_one_registry_are_each_offered_their_own_tools() {
        let registry = ToolRegistry::new();
        let has = chat_tool_set(session("has"), registry.catalog(), false, true, true, &[]);
        let lacks = chat_tool_set(session("lacks"), registry.catalog(), false, false, false, &[]);
        for name in FORUM.into_iter().chain(["context_checkpoint"]) {
            assert!(allowed(&registry, &has, name), "{name}");
            assert!(!allowed(&registry, &lacks, name), "{name}");
            assert!(registry.get(name).is_some(), "{name} stays registered");
        }
        assert!(allowed(&registry, &lacks, "read"));

        // The chat that withholds nothing sends the registry's own schema.
        assert!(registry.tools_schema_without(has.withheld()).is_none());
        let sent = registry.tools_schema_without(lacks.withheld()).unwrap();
        let whole = registry.tools_schema();
        let kept: Vec<&str> = names(&whole)
            .into_iter()
            .filter(|n| !FORUM.contains(n) && *n != "context_checkpoint")
            .collect();
        assert_eq!(names(&sent), kept, "the rest, in the registry's order");
        assert_eq!(sent.len() + 4, whole.len());

        // One of the two things each: only what that chat lacks is withheld.
        let no_forum = chat_tool_set(session("nf"), registry.catalog(), false, true, false, &[]);
        assert!(allowed(&registry, &no_forum, "context_checkpoint"));
        assert!(!allowed(&registry, &no_forum, "forum_read"));
        let no_context = chat_tool_set(session("nc"), registry.catalog(), false, false, true, &[]);
        assert!(!allowed(&registry, &no_context, "context_checkpoint"));
        assert!(allowed(&registry, &no_context, "forum_read"));
    }

    /// What `search_tools` answers a chat with this set for "forum".
    async fn search(registry: &ToolRegistry, set: SessionToolSet) -> String {
        let mut ctx = create_tool_context();
        ctx.capabilities.tool_activation = Some(ActivationCapability::new(
            registry.catalog().clone(),
            Arc::new(RwLock::new(set)),
            ActivationAuthority::ModelConfirmed,
        ));
        let found = crate::tools::SearchToolsTool.execute(json!({"query": "forum"}), ctx);
        found.await.unwrap()
    }

    /// Progressive disclosure: the forum tools are deferred, so being
    /// offered one means being able to activate it. The chat with a forum
    /// can; the chat without is told there is no such tool, and neither the
    /// deferred list nor a search shows it one.
    #[tokio::test]
    async fn a_chat_cannot_activate_or_find_what_it_is_not_offered() {
        let registry = ToolRegistry::new();
        let catalog = registry.catalog();
        let post = ToolId::builtin("forum_post");
        let mut has = chat_tool_set(session("has"), catalog, true, true, true, &[]);
        let mut lacks = chat_tool_set(session("lacks"), catalog, true, false, false, &[]);
        assert!(allowed(&registry, &has, "context_checkpoint"));
        assert!(!allowed(&registry, &lacks, "context_checkpoint"));

        let deferred = |set: &SessionToolSet| {
            let split = registry.deferred_tool_split(set, &Default::default());
            let names: Vec<String> = names(&split.deferred).into_iter().map(String::from).collect();
            names
        };
        assert!(deferred(&has).contains(&"forum_post".to_string()));
        for name in FORUM.into_iter().chain(["context_checkpoint"]) {
            assert!(!deferred(&lacks).contains(&name.to_string()), "{name}");
        }
        assert!(deferred(&lacks).contains(&"subagent_start".to_string()));

        assert!(search(&registry, has.clone()).await.contains("builtin:forum_post"));
        assert!(!search(&registry, lacks.clone()).await.contains("forum_post"));

        activate_exact_for_user(&mut has, catalog, &post).unwrap();
        assert!(allowed(&registry, &has, "forum_post"));
        let refused = activate_exact_for_user(&mut lacks, catalog, &post).unwrap_err();
        assert!(refused.to_string().contains("does not know"), "{refused}");
        assert!(!allowed(&registry, &lacks, "forum_post"));
    }

    /// The registry changes while a turn runs (another chat connects an MCP
    /// server, an extension loads) and the turn's stale set is rebuilt. The
    /// rebuilt set still withholds what the chat is not offered: with
    /// whole-registry requests the core is otherwise every registered tool.
    #[test]
    fn a_registry_change_mid_turn_offers_a_chat_nothing_it_withholds() {
        for progressive in [false, true] {
            let mut registry = ToolRegistry::new();
            let id = session("lacks");
            let lacks = chat_tool_set(id.clone(), registry.catalog(), progressive, false, false, &[]);
            registry.register(Arc::new(Late));
            assert!(lacks.is_stale(registry.catalog()));
            let (rebuilt, _) = crate::runtime::stream::rebuild_stale_tool_set(
                &lacks,
                id,
                registry.catalog(),
                progressive,
                false,
            );
            assert!(!rebuilt.is_stale(registry.catalog()));
            for name in FORUM.into_iter().chain(["context_checkpoint"]) {
                assert!(!allowed(&registry, &rebuilt, name), "{name} ({progressive})");
            }
            assert!(allowed(&registry, &rebuilt, "read"));
            if !progressive {
                assert!(allowed(&registry, &rebuilt, "late_tool"));
                let sent = registry.tools_schema_without(rebuilt.withheld()).unwrap();
                assert!(names(&sent).contains(&"late_tool"));
                assert!(!names(&sent).contains(&"context_checkpoint"));
            }
        }
    }

    fn auto() -> SharedContinuation {
        let mut state = ContinuationState::default();
        state.config.mode = agent_core::config::ContextManagementMode::Auto;
        Arc::new(Mutex::new(state))
    }

    fn note_of(state: &SharedContinuation) -> String {
        state.lock().unwrap().note.clone()
    }

    /// The one registered `context_checkpoint` serves every chat: a call
    /// writes to the rollover state its own context carries, and to no
    /// other. With no state, or the state of a chat whose context
    /// management is off, it refuses and writes nothing.
    #[tokio::test]
    async fn the_one_checkpoint_tool_writes_to_the_calling_chats_state() {
        let registry = ToolRegistry::new();
        let tool = registry.get("context_checkpoint").unwrap().clone();
        let report = |state: Option<&SharedContinuation>, note: &str| {
            let mut ctx = create_tool_context();
            ctx.capabilities.continuation = state.cloned();
            tool.execute(json!({"phase": "plan", "note": note}), ctx)
        };
        let (a, b) = (auto(), auto());
        report(Some(&a), "a: unfinished").await.unwrap();
        assert_eq!((note_of(&a), note_of(&b)), ("a: unfinished".into(), String::new()));
        report(Some(&b), "b: unfinished").await.unwrap();
        assert_eq!(
            (note_of(&a), note_of(&b)),
            ("a: unfinished".into(), "b: unfinished".into())
        );

        let refused = report(None, "nobody's").await.unwrap_err().to_string();
        assert!(refused.contains("not available in this context"), "{refused}");
        let off = SharedContinuation::default();
        let refused = report(Some(&off), "off").await.unwrap_err().to_string();
        assert!(refused.contains("context management is off"), "{refused}");
        assert_eq!(
            (note_of(&a), note_of(&b), note_of(&off)),
            ("a: unfinished".into(), "b: unfinished".into(), String::new())
        );
    }
}
