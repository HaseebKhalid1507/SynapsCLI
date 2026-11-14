//! `synaps agent`, `synaps watcher` and `synaps server` were removed. For
//! one release they still parse with any arguments, print one line pointing
//! at the daemon to stderr, and exit 2, on every platform.

use std::path::Path;
use std::process::Command;

fn synaps(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_synaps"))
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("SYNAPS_BASE_DIR", home.join(".synaps-cli"))
        .env("SYNAPS_DAEMON_AUTOSPAWN", "0")
        .env_remove("SYNAPS_PROFILE")
        .output()
        .unwrap()
}

#[test]
fn removed_subcommands_print_a_pointer_and_exit_2() {
    let home = tempfile::tempdir().unwrap();
    for (argv, name) in [
        (&["agent"][..], "agent"),
        (&["agent", "--config", "/x/config.toml", "--trigger-context", "cron"][..], "agent"),
        (&["agent", "--help"][..], "agent"),
        (&["--profile", "x", "agent"][..], "agent"),
        (&["watcher"][..], "watcher"),
        (&["watcher", "deploy", "bot", "-v"][..], "watcher"),
    ] {
        let out = synaps(home.path(), argv);
        assert_eq!(out.status.code(), Some(2), "{argv:?}");
        assert!(out.stdout.is_empty(), "{argv:?}: stdout {:?}", String::from_utf8_lossy(&out.stdout));
        let stderr = String::from_utf8_lossy(&out.stderr);
        let expected = format!(
            "`synaps {name}` was removed in {}: pin a daemon session (synaps --attach \
             --keep-warm, or synaps daemon pin) and drive it with synaps send --session <name>. \
             See https://github.com/HaseebKhalid1507/SynapsCLI/blob/dev/docs/daemon-mode.md\
             #pinning-a-session-pin\n",
            agent_core::REMOVED_IN
        );
        assert_eq!(stderr, expected, "{argv:?}");
    }
}

/// `synaps completions` must not offer a removed subcommand. The fish
/// script has one `__fish_synaps_needs_command` line per top-level
/// subcommand, so its names are the list the other shells get too.
#[test]
fn completions_do_not_offer_the_removed_subcommands() {
    let home = tempfile::tempdir().unwrap();
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let out = synaps(home.path(), &["completions", shell]);
        assert!(out.status.success(), "{shell}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(!out.stdout.is_empty(), "{shell}");
    }
    let fish = String::from_utf8(synaps(home.path(), &["completions", "fish"]).stdout).unwrap();
    let top_level: Vec<&str> = fish
        .lines()
        .filter(|l| l.contains("-n \"__fish_synaps_needs_command\" -f -a \""))
        .filter_map(|l| l.split("-f -a \"").nth(1)?.split('"').next())
        .collect();
    assert!(top_level.contains(&"daemon"), "{top_level:?}");
    for removed in ["agent", "watcher", "server"] {
        assert!(!top_level.contains(&removed), "{removed} offered: {top_level:?}");
    }
}

/// `synaps server` takes any of its old flags, and the notice comes before
/// the profile check. It boots nothing: no token file, session or log is
/// written under HOME.
#[test]
fn server_points_at_the_daemon_for_any_args() {
    let expected = format!(
        "`synaps server` was removed in {}: the session daemon is the API (synaps daemon; \
         protocol: https://github.com/HaseebKhalid1507/SynapsCLI/blob/dev/docs/daemon-mode.md). \
         For a browser UI use synaps-dash. See CHANGELOG.\n",
        agent_core::REMOVED_IN
    );
    for (argv, profile_env) in [
        (&["server"][..], None),
        (&["server", "--port", "3145", "--host", "0.0.0.0"][..], None),
        (&["server", "--token", "", "--auto-approve-confirms", "--allowed-origins", "a,b"][..], None),
        (&["server", "-p", "1", "-s", "prompt.md", "--continue", "abc"][..], None),
        (&["server", "--help"][..], None),
        (&["server", "extra", "positional", "--bogus=1"][..], None),
        (&["--profile", "b", "server", "--port", "1"][..], Some("a")),
    ] {
        let home = tempfile::tempdir().unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_synaps"));
        cmd.args(argv)
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("SYNAPS_BASE_DIR", home.path().join(".synaps-cli"))
            .env("SYNAPS_DAEMON_AUTOSPAWN", "0")
            .env_remove("SYNAPS_PROFILE");
        if let Some(profile) = profile_env {
            cmd.env("SYNAPS_PROFILE", profile);
        }
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{argv:?}");
        assert!(out.stdout.is_empty(), "{argv:?}: stdout {:?}", String::from_utf8_lossy(&out.stdout));
        assert_eq!(String::from_utf8_lossy(&out.stderr), expected, "{argv:?}");
        let written: Vec<_> = std::fs::read_dir(home.path()).unwrap().collect();
        assert!(written.is_empty(), "{argv:?}: wrote {written:?}");
    }
}
