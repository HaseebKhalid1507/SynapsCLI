// The opt-out gate (slice 8b): included at the end of launch_tests.rs as
// `mod optout_gate`. The parameter schemas of subagent_start,
// subagent_resume and subagent (the oneshot), byte for byte. While worker
// rollover was off they were what they were before the `context_rollover`
// parameter: no model saw a parameter that would do nothing, and no tool
// catalog changed. Fixtures in tests/fixtures/optout_gate/ were written with
// OPTOUT_GATE_WRITE=1 at the slice-8b base (0ed5f2cb), twice, with no
// difference between the two writes. Nothing is normalized.
// Provenance on the dev port: the fixtures are re-pinned (e68127e6) to pin
// A, 4dbbdcb2, written with OPTOUT_GATE_WRITE=1 at a7e1f463 (the dev base
// 7fdd5a4d plus these gate files, no production change), twice, the second
// write identical to the first. The 0ed5f2cb recording is history.
// Slice 9 (worker rollover on): re-pinned to the slice 9 schemas by an edit
// of each fixture, not by a write; the only change to each is the optional
// `context_rollover` property (`x4`). This check passing at the slice 9 tree
// shows each fixture is byte for byte what OPTOUT_GATE_WRITE=1 would write.
// Subagent thinking (fdb275f4, train 26.3): re-pinned by an edit of each
// fixture, not by a write; the only change to each is the optional
// `thinking` property (start and oneshot "Worker effort override; …",
// resume "Override resumed worker effort. …"). This check passing at the
// re-pin's tree (3434f892 plus the edited fixtures, no production change)
// shows each fixture is byte for byte what OPTOUT_GATE_WRITE=1 would write.

use crate::tools::Tool;

const OPTOUT_GATE_DIR: &str = "tests/fixtures/optout_gate";

fn optout_gate_check(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(OPTOUT_GATE_DIR)
        .join(name);
    if std::env::var_os("OPTOUT_GATE_WRITE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(
        expected == actual,
        "{name} differs from the fixture:\n  fixture: {expected}\n  now:     {actual}"
    );
}

#[test]
fn optout_gate_the_worker_tools_schemas_are_pinned() {
    let tools: [(&str, &dyn Tool); 3] = [
        ("start", &crate::tools::subagent::SubagentStartTool),
        ("resume", &crate::tools::subagent::SubagentResumeTool),
        ("oneshot", &crate::tools::subagent::SubagentTool),
    ];
    for (name, tool) in tools {
        // The exact bytes a provider receives (compact) and a readable copy.
        let schema = tool.parameters();
        optout_gate_check(&format!("{name}_schema.json"), &schema.to_string());
        optout_gate_check(
            &format!("{name}_schema_pretty.json"),
            &serde_json::to_string_pretty(&schema).unwrap(),
        );
    }
}
// Shown red, while dark, by a mutation that added the parameter whatever
// the build switch said (each *_schema.json differed); since slice 9 by
// building it off (each differs by the parameter).
