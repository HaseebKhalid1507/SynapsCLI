// Slice 8b, opt-out (spec §2): `context_rollover: "inherit" | "off"` on
// subagent_start, subagent_resume and subagent (the oneshot), and the
// operator key `subagent.context_rollover`. Included at the end of
// launch_tests.rs as `mod optout`. Since slice 9 the parameter is in the
// schemas (`optout_gate`) and read before anything starts; while worker
// rollover was off it was neither, and a value given anyway was ignored like
// any unknown key. Its check is tested through `context_rollover_param` with
// the switch passed in.

use agent_core::config::ContextRollover;
use crate::tools::subagent::{SubagentResumeTool, SubagentStartTool, SubagentTool};
use crate::tools::Tool;

fn rollover_param(value: Value) -> Value {
    json!({ "context_rollover": value })
}

#[test]
fn x1_context_rollover_is_inherit_or_off_and_nothing_else() {
    for (params, want) in [
        (json!({}), ContextRollover::Inherit),
        (rollover_param(Value::Null), ContextRollover::Inherit),
        (rollover_param(json!("inherit")), ContextRollover::Inherit),
        (rollover_param(json!("off")), ContextRollover::Off),
    ] {
        assert_eq!(context_rollover_param(&params, true), Ok(want), "{params}");
        assert_eq!(
            context_rollover_param(&params, false),
            Ok(ContextRollover::Inherit),
            "{params}: not read while off"
        );
    }
    for bad in [
        json!("OFF"),
        json!("Inherit"),
        json!(""),
        json!(" off"),
        json!("on"),
        json!("auto"),
        json!(true),
        json!(0),
        json!({}),
        json!(["off"]),
    ] {
        assert_eq!(
            context_rollover_param(&rollover_param(bad.clone()), true),
            Err(CONTEXT_ROLLOVER_VALUES.to_owned()),
            "{bad}"
        );
        assert_eq!(
            context_rollover_param(&rollover_param(bad.clone()), false),
            Ok(ContextRollover::Inherit),
            "{bad}: not read while off"
        );
    }
    assert_eq!(CONTEXT_ROLLOVER_VALUES, "context_rollover must be \"inherit\" or \"off\"");
}

/// The three tools' calls: a fresh-start, a resume and a oneshot, each with
/// `rollover` and nothing that would start a worker past the check (no
/// agent or system prompt; no registry for the resume).
fn calls(rollover: Value) -> [(&'static str, &'static dyn Tool, Value); 3] {
    [
        (
            "subagent_start",
            &SubagentStartTool,
            json!({"task": "t", "context_rollover": rollover.clone()}),
        ),
        (
            "subagent_resume",
            &SubagentResumeTool,
            json!({"handle_id": "sa_1", "instructions": "go on", "context_rollover": rollover.clone()}),
        ),
        (
            "subagent",
            &SubagentTool,
            json!({"task": "t", "context_rollover": rollover}),
        ),
    ]
}

/// With worker rollover on (slice 9) the three tools read the parameter
/// first: a bad value is refused before anything starts
/// (`context_rollover_param` with the switch on, `x1`), and "inherit",
/// "off" and null give the outcome the same call has without it.
#[tokio::test]
async fn x2_all_three_tools_refuse_a_bad_value_before_anything_starts() {
    for value in [json!("sometimes"), json!(false), json!("inherit"), json!("off"), Value::Null] {
        for (name, tool, params) in calls(value.clone()) {
            let mut without = params.clone();
            without.as_object_mut().unwrap().remove("context_rollover");
            let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
            let mut ctx = crate::tools::test_helpers::create_tool_context();
            ctx.capabilities.subagent_registry = Some(Arc::clone(&registry));
            let err = tool.execute(params, ctx).await.expect_err(name).to_string();
            assert!(registry.lock().unwrap().list_active().is_empty(), "{name}: nothing started");
            let bad = !matches!(value.as_str(), Some("inherit" | "off")) && !value.is_null();
            assert_eq!(err.contains(CONTEXT_ROLLOVER_VALUES), bad, "{name} {value}: {err}");
            if bad {
                continue;
            }
            let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
            let mut ctx = crate::tools::test_helpers::create_tool_context();
            ctx.capabilities.subagent_registry = Some(Arc::clone(&registry));
            let as_before = tool.execute(without, ctx).await.expect_err(name).to_string();
            assert!(!err.contains(CONTEXT_ROLLOVER_VALUES), "{name} {value}: {err}");
            assert_eq!(err, as_before, "{name} {value}: as without the parameter");
        }
    }
}

/// Only a parent whose live mode is auto, with neither the operator nor the
/// caller opting out, gives a worker rollover, and since slice 9 a plan.
#[test]
fn x3_off_from_the_operator_or_the_caller_wins_over_inherit() {
    use ContextRollover::{Inherit, Off};
    let auto = auto_policy();
    let operator_off = crate::tools::WorkerContextPolicy {
        operator_rollover: Off,
        ..auto_policy()
    };
    let mode_off = crate::tools::WorkerContextPolicy::default();
    for (policy, choice, want) in [
        (Some(&auto), Inherit, true),
        (Some(&auto), Off, false),
        (Some(&operator_off), Inherit, false),
        (Some(&operator_off), Off, false),
        (Some(&mode_off), Inherit, false),
        (Some(&mode_off), Off, false),
        (None, Inherit, false),
        (None, Off, false),
    ] {
        assert_eq!(rollover_wanted(policy, choice), want, "{policy:?} {choice:?}");
        assert_eq!(wants_rollover(policy, choice), want, "a plan: {policy:?} {choice:?}");
    }
    assert_eq!(
        crate::tools::WorkerContextPolicy::default().operator_rollover,
        Inherit,
        "no operator key: inherit"
    );
}

/// With worker rollover on (slice 9), the three schemas offer the
/// parameter, never required, and nothing else in them changes: without
/// it each is the schema `with_rollover_param` leaves alone when off.
#[test]
fn x4_the_schemas_offer_the_parameter() {
    let tools: [&dyn Tool; 3] = [&SubagentStartTool, &SubagentResumeTool, &SubagentTool];
    for tool in tools {
        let live = tool.parameters();
        let param = &live["properties"]["context_rollover"];
        assert_eq!(param["type"], "string", "{}", tool.name());
        assert_eq!(param["enum"], json!(["inherit", "off"]));
        let description = param["description"].as_str().unwrap();
        assert!(description.contains("\"off\""), "{description}");
        let mut dark = live.clone();
        dark["properties"].as_object_mut().unwrap().remove("context_rollover");
        assert_eq!(with_rollover_param(dark.clone(), false), dark, "{}", tool.name());
        assert_eq!(with_rollover_param(dark.clone(), true), live, "{}: only the parameter is added", tool.name());
        let mut required = live["required"].as_array().into_iter().flatten();
        assert!(!required.any(|r| r == "context_rollover"), "never required");
    }
}
