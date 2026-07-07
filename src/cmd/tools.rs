//! `synaps tools` — tool-surface utilities.
//!
//! Currently exposes one subcommand:
//!   `synaps tools export` — emit the full tool-schema manifest as JSON to stdout.
//!
//! The manifest is an array of `{ name, description, parameters }` objects —
//! one entry per registered tool (builtin + extension + MCP-bridged).  This is
//! the "agent-legible API" described in the field report (P13).

use anyhow::Result;
use serde_json::{json, Value};
use agent_engine::tools::ToolRegistry;

/// Subcommand dispatcher for `synaps tools <subcommand>`.
pub async fn run(action: ToolsAction) -> Result<()> {
    match action {
        ToolsAction::Export { pretty } => export(pretty).await,
    }
}

#[derive(clap::Subcommand, Debug)]
pub enum ToolsAction {
    /// Export all tool schemas as a JSON manifest to stdout.
    ///
    /// Each entry has the shape:
    ///   { "name": "...", "description": "...", "parameters": { <JSON Schema> } }
    ///
    /// Covers builtin tools + runtime-registered extension tools + MCP-bridged tools.
    /// MCP tools are not connected at export time (they require a live subprocess);
    /// their schemas appear in the manifest only if they have been registered via
    /// `ToolRegistry::register()` before export runs.  The committed docs/tools.json
    /// captures the builtin surface, which is the stable contract.
    Export {
        /// Pretty-print the JSON output (2-space indent).
        #[arg(long, short = 'p')]
        pretty: bool,
    },
}

async fn export(pretty: bool) -> Result<()> {
    let registry = ToolRegistry::new();

    let manifest: Vec<Value> = registry
        .iter_tools_sorted()
        .into_iter()
        .map(|tool| {
            json!({
                "name":        tool.name(),
                "description": tool.description(),
                "parameters":  tool.parameters(),
            })
        })
        .collect();

    let out = if pretty {
        serde_json::to_string_pretty(&manifest)?
    } else {
        serde_json::to_string(&manifest)?
    };

    println!("{out}");
    Ok(())
}
