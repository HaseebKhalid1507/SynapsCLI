//! Structural guards for the explicit conversation-boundary call sites.
//! Latch behavior and clone isolation are covered by Runtime unit tests.

#[test]
fn session_approval_new_session_and_resume_use_explicit_boundary() {
    let actor = include_str!("../crates/agent-engine/src/session/actor.rs");
    let new_session = actor
        .split("SessionCommand::NewSession => {")
        .nth(1)
        .unwrap()
        .split("SessionCommand::Save =>")
        .next()
        .unwrap();
    assert!(new_session.contains(".begin_conversation(Some(self.conv.session.id.clone()))"));

    let cmds = include_str!("../crates/agent-engine/src/session/actor_cmds.rs");
    let resume = cmds
        .split("pub(crate) async fn resume(")
        .nth(1)
        .unwrap()
        .split("pub(crate) fn emit_subagent_rows")
        .next()
        .unwrap();
    let boundary = resume
        .find(".begin_conversation(Some(new_id.clone()))")
        .unwrap();
    // Failed lookup and the streaming refusal return before the boundary;
    // successful replacement (including same-ID resume) refreshes consent.
    assert!(resume.find("cannot resume while streaming").unwrap() < boundary);
    assert!(
        resume
            .find("let session = match crate::resolve_session")
            .unwrap()
            < boundary
    );
    assert!(
        resume
            .find("ConversationState::from_resumed(session)")
            .unwrap()
            < boundary
    );
    assert_eq!(actor.matches(".begin_conversation(").count(), 1);
    assert_eq!(cmds.matches(".begin_conversation(").count(), 1);
    assert!(actor.contains("self.runtime.set_session_id(Some(new_id.clone()))"));
}

#[test]
fn session_approval_live_headless_clear_uses_explicit_boundary() {
    // Feature-gated legacy_inline remains reachable; default chat uses actor.
    let chat = include_str!("../src/cmd/chat.rs");
    let clear = chat
        .split("\"clear\" => {")
        .nth(1)
        .unwrap()
        .split("\"sessions\" =>")
        .next()
        .unwrap();
    assert!(clear.contains("runtime.begin_conversation(Some(conv.session.id.clone()))"));
}
