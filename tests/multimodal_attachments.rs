//! Local attachment bytes survive persistence without the original file. All
//! tests use synthetic files/tempdirs and never invoke a provider or live memory.
use std::sync::Arc;
use synaps_cli::core::session_journal::{save_session_in_dir, SessionPersistence};
use synaps_cli::{attachments, Session};

#[tokio::test]
async fn attachment_json_and_journal_roundtrip_after_original_removed() {
    for mode in [SessionPersistence::Json, SessionPersistence::Journal] {
        let tmp = tempfile::tempdir().unwrap();
        let selected = tmp.path().join("selected.txt");
        std::fs::write(&selected, "SYNTHETIC_SELECTED_FILE_BYTES").unwrap();
        let content = attachments::build_user_content("review it", std::slice::from_ref(&selected))
            .await
            .unwrap();
        let mut session = Session::new("openai-codex/gpt-6-astra", "medium", None);
        session.api_messages.push(Arc::new(
            serde_json::json!({"role":"user","content":"earlier"}),
        ));
        let dir = tmp.path().join("sessions");
        save_session_in_dir(&dir, &session, mode).unwrap();
        session.api_messages.push(Arc::new(
            serde_json::json!({"role":"user","content":content}),
        ));
        save_session_in_dir(&dir, &session, mode).unwrap();
        std::fs::remove_file(selected).unwrap();
        let loaded = Session::load_from_dir(&dir, &session.id).unwrap();
        assert_eq!(loaded.api_messages, session.api_messages);
        assert_eq!(
            loaded.api_messages[1]["content"][1]["source"]["data"],
            "SYNTHETIC_SELECTED_FILE_BYTES"
        );
        synaps_cli::runtime::attachments::validate_messages(&loaded.model, &loaded.api_messages)
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.join(format!("{}.json", session.id)))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
