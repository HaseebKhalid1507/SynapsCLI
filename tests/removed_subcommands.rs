//! `synaps agent` and `synaps watcher` were removed. For one
//! release they still parse with any arguments, print one line pointing at
//! the daemon to stderr, and exit 2, on every platform.

use std::process::Command;

#[test]
fn removed_subcommands_print_a_pointer_and_exit_2() {
    let home = tempfile::tempdir().unwrap();
    for (argv, name) in [
        (&["agent"][..], "agent"),
        (&["agent", "--config", "/x/config.toml", "--trigger-context", "cron"][..], "agent"),
        (&["agent", "--help"][..], "agent"),
        (&["watcher"][..], "watcher"),
        (&["watcher", "deploy", "bot", "-v"][..], "watcher"),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_synaps"))
            .args(argv)
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("SYNAPS_BASE_DIR", home.path().join(".synaps-cli"))
            .env("SYNAPS_DAEMON_AUTOSPAWN", "0")
            .env_remove("SYNAPS_PROFILE")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{argv:?}");
        assert!(out.stdout.is_empty(), "{argv:?}: stdout {:?}", String::from_utf8_lossy(&out.stdout));
        let stderr = String::from_utf8_lossy(&out.stderr);
        let expected = format!(
            "`synaps {name}` was removed in 0.12.0: run long-lived headless work as a pinned \
             daemon session (synaps daemon, synaps --attach --keep-warm). See CHANGELOG.\n"
        );
        assert_eq!(stderr, expected, "{argv:?}");
    }
}
