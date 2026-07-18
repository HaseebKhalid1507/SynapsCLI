//! P13 integration tests for `synaps tools export`.
//!
//! Spawns the real `synaps` binary (via CARGO_BIN_EXE_synaps) and asserts
//! the contract described in the P13 acceptance criteria:
//!   - Output is valid JSON
//!   - Contains all 18 builtin tools
//!   - Each tool's parameters is a valid JSON Schema (object-typed, has `properties`)
//!   - Output is deterministic across runs (byte-identical)
//!   - `--pretty` output matches the committed docs/tools.json (drift-check contract)
//!   - Output ends with a newline (POSIX convention)
//!
//! Written by Shady (subagent, code review / test authorship).
//! Branch: feat/tui-headless-harness  (P13 coverage pass)

use std::process::Command;

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Run `synaps tools export` with the given extra args. Returns stdout as String.
/// Panics with full stderr if the command exits non-zero.
fn run_export(extra_args: &[&str]) -> String {
    let bin = env!("CARGO_BIN_EXE_synaps");
    let mut cmd = Command::new(bin);
    cmd.arg("tools").arg("export");
    for arg in extra_args {
        cmd.arg(arg);
    }
    // Provide a clean HOME so we never hit real user config or live MCP processes.
    let tmp = tempfile::tempdir().expect("tempdir");
    cmd.env("HOME", tmp.path())
        .env_remove("ANTHROPIC_API_KEY");

    let output = cmd.output().expect("failed to spawn synaps binary");

    if !output.status.success() {
        panic!(
            "`synaps tools export {:?}` failed:\nstatus: {}\nstderr: {}",
            extra_args,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    String::from_utf8(output.stdout).expect("synaps tools export output is valid UTF-8")
}

/// The 18 builtin tool names from docs/tools.json (alphabetical, authoritative).
const EXPECTED_TOOL_NAMES: &[&str] = &[
    "bash",
    "edit",
    "find",
    "grep",
    "ls",
    "read",
    "shell_end",
    "shell_send",
    "shell_start",
    "subagent",
    "subagent_collect",
    "subagent_model_authorize",
    "subagent_models",
    "subagent_resume",
    "subagent_start",
    "subagent_status",
    "subagent_steer",
    "write",
];

// ─── Test 1: Output is valid JSON ─────────────────────────────────────────────

#[test]
fn export_produces_valid_json() {
    let out = run_export(&[]);
    serde_json::from_str::<serde_json::Value>(&out)
        .expect("`synaps tools export` output must be parseable as valid JSON");
}

#[test]
fn export_pretty_produces_valid_json() {
    let out = run_export(&["--pretty"]);
    serde_json::from_str::<serde_json::Value>(&out)
        .expect("`synaps tools export --pretty` output must be parseable as valid JSON");
}

// ─── Test 2: Output contains all 18 builtin tools ────────────────────────────

#[test]
fn export_contains_all_18_builtin_tools() {
    let out = run_export(&[]);
    let manifest: Vec<serde_json::Value> = serde_json::from_str(&out)
        .expect("output must be a JSON array");

    assert_eq!(manifest.len(), 18,
        "export must contain exactly 18 builtin tools, got {}: {:?}",
        manifest.len(),
        manifest.iter().filter_map(|t| t["name"].as_str()).collect::<Vec<_>>()
    );

    let names: Vec<&str> = manifest.iter()
        .filter_map(|t| t["name"].as_str())
        .collect();

    for expected in EXPECTED_TOOL_NAMES {
        assert!(names.contains(expected),
            "tool '{}' is missing from the export manifest", expected);
    }
}

// ─── Test 3: Each tool's parameters is a valid JSON Schema ───────────────────

#[test]
fn export_each_tool_parameters_is_valid_json_schema() {
    let out = run_export(&[]);
    let manifest: Vec<serde_json::Value> = serde_json::from_str(&out)
        .expect("output must be a JSON array");

    for entry in &manifest {
        let name = entry["name"].as_str().unwrap_or("<unknown>");
        let params = &entry["parameters"];

        assert!(params.is_object(),
            "tool '{}': 'parameters' must be a JSON object", name);

        let schema_type = params["type"].as_str();
        assert_eq!(schema_type, Some("object"),
            "tool '{}': parameters schema must have {{\"type\": \"object\"}}", name);

        assert!(params["properties"].is_object(),
            "tool '{}': parameters schema must have a 'properties' object", name);

        // If there are required fields, they must all appear in properties.
        if let Some(required) = params["required"].as_array() {
            let properties = params["properties"].as_object()
                .expect("properties must be an object");
            for req in required {
                let field = req.as_str().expect("required entries must be strings");
                assert!(properties.contains_key(field),
                    "tool '{}': required field '{}' is missing from 'properties'", name, field);
            }
        }
    }
}

// ─── Test 4: Output is deterministic across runs (byte-identical) ─────────────

#[test]
fn export_is_deterministic_across_runs() {
    let first  = run_export(&[]);
    let second = run_export(&[]);

    assert_eq!(first, second,
        "`synaps tools export` must produce byte-identical output across runs (deterministic ordering)");
}

#[test]
fn export_pretty_is_deterministic_across_runs() {
    let first  = run_export(&["--pretty"]);
    let second = run_export(&["--pretty"]);

    assert_eq!(first, second,
        "`synaps tools export --pretty` must produce byte-identical output across runs");
}

// ─── Test 5: --pretty output is byte-identical to committed docs/tools.json ───
//
// This IS the drift-check contract at the Rust test layer.
// If this fails, the committed snapshot is stale — run:
//   synaps tools export --pretty > docs/tools.json
// and commit the update.

#[test]
fn export_pretty_matches_committed_docs_tools_json() {
    // Locate docs/tools.json relative to the workspace root (CARGO_MANIFEST_DIR
    // for the synaps root crate is the workspace root when running `cargo test -p synaps`).
    // We walk up from the test binary's manifest dir to find the workspace root.
    let committed_path = {
        // CARGO_MANIFEST_DIR is set at compile time to the crate's Cargo.toml directory.
        // For `tests/tools_export.rs`, that is the synaps root crate (workspace root).
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        std::path::PathBuf::from(manifest_dir).join("docs").join("tools.json")
    };

    assert!(committed_path.exists(),
        "docs/tools.json not found at {:?} — was it moved or deleted?", committed_path);

    let committed = std::fs::read_to_string(&committed_path)
        .expect("failed to read docs/tools.json");

    let generated = run_export(&["--pretty"]);

    // The drift-check script uses `diff <(echo "$GENERATED") "$COMMITTED"`.
    // `echo` appends a trailing newline; `println!` in Rust does the same.
    // Both sides should end with a single newline — if this assert fires,
    // re-run `synaps tools export --pretty > docs/tools.json`.
    assert_eq!(generated, committed,
        "DRIFT DETECTED: `synaps tools export --pretty` output does not match docs/tools.json.\n\
         Fix: run `synaps tools export --pretty > docs/tools.json` and commit."
    );
}

// ─── Test 6: JSON output ends with a newline (POSIX convention) ───────────────

#[test]
fn export_output_ends_with_newline() {
    let out = run_export(&[]);
    assert!(out.ends_with('\n'),
        "`synaps tools export` output must end with a newline (POSIX convention)");
}

#[test]
fn export_pretty_output_ends_with_newline() {
    let out = run_export(&["--pretty"]);
    assert!(out.ends_with('\n'),
        "`synaps tools export --pretty` output must end with a newline (POSIX convention)");
}

// ─── Test 7: Output is a JSON array (not object, not primitive) ───────────────

#[test]
fn export_output_is_json_array() {
    let out = run_export(&[]);
    let val: serde_json::Value = serde_json::from_str(&out)
        .expect("output must be valid JSON");
    assert!(val.is_array(),
        "`synaps tools export` must emit a JSON array at the top level, got: {:?}",
        val.as_object().map(|o| format!("object with {} keys", o.len()))
            .unwrap_or_else(|| "non-array, non-object value".to_string())
    );
}

// ─── Test 8: Each entry has exactly the documented shape ─────────────────────
//
// Shape contract from P13 spec: {name: string, description: string, parameters: object}
// The export format must NOT include internal registry fields like `input_schema`
// (which is the Anthropic API shape) — it exposes the raw `parameters()` output.

#[test]
fn export_each_entry_has_name_description_parameters_shape() {
    let out = run_export(&[]);
    let manifest: Vec<serde_json::Value> = serde_json::from_str(&out)
        .expect("output must be a JSON array");

    for entry in &manifest {
        let obj = entry.as_object()
            .expect("each manifest entry must be a JSON object");

        assert!(obj.contains_key("name"),        "entry missing 'name' key: {:?}", entry);
        assert!(obj.contains_key("description"), "entry missing 'description' key: {:?}", entry);
        assert!(obj.contains_key("parameters"),  "entry missing 'parameters' key: {:?}", entry);

        let name = obj["name"].as_str().unwrap_or("");
        assert!(!name.is_empty(), "entry 'name' must be a non-empty string");

        let desc = obj["description"].as_str().unwrap_or("");
        assert!(!desc.is_empty(), "tool '{}': 'description' must be a non-empty string", name);

        // NOTE: we assert parameters is NOT the Anthropic API shape (which uses `input_schema`).
        // The export uses the raw Tool::parameters() which produces `parameters`.
        assert!(!obj.contains_key("input_schema"),
            "tool '{}': export must use 'parameters' key (raw Tool trait output), \
             not 'input_schema' (Anthropic API shape) — wrong schema source?", name);
    }
}

// ─── Test 9: Tools appear in alphabetical order in the output ─────────────────

#[test]
fn export_tools_are_in_alphabetical_order() {
    let out = run_export(&[]);
    let manifest: Vec<serde_json::Value> = serde_json::from_str(&out)
        .expect("output must be a JSON array");

    let names: Vec<&str> = manifest.iter()
        .filter_map(|t| t["name"].as_str())
        .collect();

    let mut sorted = names.clone();
    sorted.sort();

    assert_eq!(names, sorted,
        "export output must list tools in alphabetical order; \
         got {:?}, expected {:?}", names, sorted);
}

// ─── Test 10: Drift-check script exits 0 when binary matches committed JSON ───
//
// Spawns the actual shell script to verify the shell-level contract (not just
// the Rust-level byte comparison above).  Skipped if `bash` is not in PATH.

#[test]
fn drift_check_script_exits_zero_when_no_drift() {
    let bash = std::process::Command::new("which")
        .arg("bash")
        .output()
        .ok()
        .filter(|o| o.status.success());

    if bash.is_none() {
        eprintln!("SKIP: bash not available — skipping drift-check script test");
        return;
    }

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let script = std::path::PathBuf::from(manifest_dir)
        .join("scripts")
        .join("tools-schema-drift-check.sh");

    if !script.exists() {
        eprintln!("SKIP: scripts/tools-schema-drift-check.sh not found");
        return;
    }

    let bin = env!("CARGO_BIN_EXE_synaps");

    let status = std::process::Command::new("bash")
        .arg(&script)
        .env("SYNAPS_BIN", bin)
        .env("COMMITTED", std::path::PathBuf::from(manifest_dir).join("docs").join("tools.json"))
        .status()
        .expect("failed to run drift-check script");

    assert!(status.success(),
        "scripts/tools-schema-drift-check.sh must exit 0 when binary matches committed JSON, \
         got exit code: {:?}", status.code());
}

// ─── Test 11: Drift-check script exits non-zero when committed JSON is modified

#[test]
fn drift_check_script_exits_nonzero_when_drift_detected() {
    let bash = std::process::Command::new("which")
        .arg("bash")
        .output()
        .ok()
        .filter(|o| o.status.success());

    if bash.is_none() {
        eprintln!("SKIP: bash not available — skipping drift-check script test");
        return;
    }

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let script = std::path::PathBuf::from(manifest_dir)
        .join("scripts")
        .join("tools-schema-drift-check.sh");

    if !script.exists() {
        eprintln!("SKIP: scripts/tools-schema-drift-check.sh not found");
        return;
    }

    // Write a deliberately wrong "committed" file to a tempfile.
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    std::fs::write(tmp.path(), b"[{\"name\":\"fake_tool\",\"description\":\"not real\",\"parameters\":{\"type\":\"object\",\"properties\":{}}}]\n")
        .expect("write fake committed JSON");

    let bin = env!("CARGO_BIN_EXE_synaps");

    let status = std::process::Command::new("bash")
        .arg(&script)
        .env("SYNAPS_BIN", bin)
        .env("COMMITTED", tmp.path())
        .status()
        .expect("failed to run drift-check script");

    assert!(!status.success(),
        "scripts/tools-schema-drift-check.sh must exit non-zero when committed JSON diverges \
         from binary output, but it exited 0 — drift detection is broken");
}
