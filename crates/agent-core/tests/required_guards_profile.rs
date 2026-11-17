//! `required_guards` under a profile.
//!
//! With a profile active, the config file the product reads is the
//! profile's when it exists and the default file otherwise. The product's
//! first config write under a profile creates the profile's file holding that
//! one key, so every setting then comes from it. The required guards must
//! not: a list in the default file keeps gating, whatever else is written.
//!
//! One test, alone in its own binary: the profile is process-wide and set
//! once.

use agent_core::config::{
    load_config, read_required_guards, set_base_dir_for_tests, set_profile, write_config_value,
};

#[test]
fn a_profiles_first_config_write_does_not_drop_the_default_files_required_guards() {
    let base = tempfile::tempdir().expect("tempdir");
    set_base_dir_for_tests(base.path().to_path_buf());
    set_profile(Some("work".to_string()));
    let default_file = base.path().join("config");
    let profile_file = base.path().join("work").join("config");

    std::fs::write(
        &default_file,
        "required_guards = example-guard\nmodel = claude-opus-4-6\n",
    )
    .expect("write config");
    assert!(!profile_file.exists());
    assert_eq!(load_config().required_guards, ["example-guard"]);
    assert_eq!(read_required_guards(), ["example-guard"]);

    // The first write under the profile: its file holds this one key, and
    // the settings now come from it...
    write_config_value("model", "claude-sonnet-4-6").expect("write");
    assert_eq!(
        std::fs::read_to_string(&profile_file).expect("profile file"),
        "model = claude-sonnet-4-6\n"
    );
    let config = load_config();
    assert_eq!(config.model.as_deref(), Some("claude-sonnet-4-6"));
    // ...and the guard list still comes from the default file as well.
    assert_eq!(config.required_guards, ["example-guard"]);
    assert_eq!(read_required_guards(), ["example-guard"]);

    // The profile can add a guard, never remove the default file's: both
    // lists apply, the default file's first, once each.
    std::fs::write(
        &profile_file,
        "model = claude-sonnet-4-6\nrequired_guards = extra, example-guard\n",
    )
    .expect("write profile config");
    assert_eq!(load_config().required_guards, ["example-guard", "extra"]);
    assert_eq!(read_required_guards(), ["example-guard", "extra"]);
    std::fs::write(&profile_file, "required_guards =\n").expect("write profile config");
    assert_eq!(load_config().required_guards, ["example-guard"]);

    // A guard only the profile's file names gates too.
    std::fs::write(&default_file, "model = claude-opus-4-6\n").expect("write config");
    std::fs::write(&profile_file, "required_guards = extra\n").expect("write profile config");
    assert_eq!(load_config().required_guards, ["extra"]);
    assert_eq!(read_required_guards(), ["extra"]);
}
