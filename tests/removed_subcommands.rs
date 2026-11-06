//! `synaps agent` and `synaps watcher` were removed. For one
//! release they still parse with any arguments, print one line pointing at
//! the daemon to stderr, and exit 2, on every platform.

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
    for removed in ["agent", "watcher"] {
        assert!(!top_level.contains(&removed), "{removed} offered: {top_level:?}");
    }
}
