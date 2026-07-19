//! `synaps trace` — trace export utilities (Task 12).
//!
//! - `synaps trace export <turn-or-request-id> --metadata-only --output PATH`
//!   selects schema-validated `synaps-request-trace/1` records from the
//!   private trace log and writes them to a fresh private (`0600`) file.
//! - `--include-content` additionally requires the explicit
//!   `--allow-content-export` opt-in **in the same invocation** and an
//!   existing ephemeral capture bundle produced by `/trace next content`
//!   in a live session; the export is recursively redacted, written under
//!   its own `synaps-trace-content-export/1` schema, and the capture
//!   bundle is consumed. Fail-closed everywhere; no network access.

use agent_engine::runtime::trace;
use anyhow::{bail, Result};
use std::path::PathBuf;

#[derive(clap::Subcommand, Debug)]
pub enum TraceAction {
    /// Export persisted trace records for one turn or request ID.
    Export {
        /// Turn or request ID (as shown in trace records / error output).
        id: String,
        /// Export metadata-only request-trace records (the default mode).
        #[arg(long)]
        metadata_only: bool,
        /// Export the redacted content capture for this request instead of
        /// metadata records. Requires --allow-content-export and a capture
        /// armed via `/trace next content` in a live session.
        #[arg(long)]
        include_content: bool,
        /// Explicit opt-in acknowledging that (redacted) request content
        /// leaves the private capture store. Required by --include-content.
        #[arg(long)]
        allow_content_export: bool,
        /// Destination file (created fresh, mode 0600, parent 0700).
        #[arg(long, short = 'o')]
        output: PathBuf,
    },
}

pub fn run(action: TraceAction) -> Result<()> {
    // Opportunistic expired-capture sweep (B2): expiry is enforced
    // logically at export (an expired bundle can never be exported); this
    // physically removes stale bundles on every trace CLI interaction —
    // no background process exists once sessions exit.
    let _ = trace::sweep_expired_captures(&trace::default_capture_dir());
    match action {
        TraceAction::Export {
            id,
            metadata_only,
            include_content,
            allow_content_export,
            output,
        } => export(
            id,
            metadata_only,
            include_content,
            allow_content_export,
            output,
        ),
    }
}

fn export(
    id: String,
    metadata_only: bool,
    include_content: bool,
    allow_content_export: bool,
    output: PathBuf,
) -> Result<()> {
    if metadata_only && include_content {
        bail!("--metadata-only and --include-content are mutually exclusive");
    }
    if include_content {
        if !allow_content_export {
            bail!(
                "--include-content requires the explicit --allow-content-export \
                 opt-in in the same invocation.\n\
                 WARNING: content export writes (redacted) request content to disk. \
                 Persisted traces never contain content; only a capture armed with \
                 `/trace next content` in a live session can be exported."
            );
        }
        eprintln!(
            "WARNING: exporting redacted request content for {id}. The output is \
             private (0600) and recursively redacted, but treat it as sensitive."
        );
        trace::export_content(
            &trace::default_capture_dir(),
            &id,
            &output,
            allow_content_export,
        )?;
        eprintln!("content export written to {}", output.display());
        return Ok(());
    }
    // Default is metadata-only (also when --metadata-only is passed
    // explicitly): the persisted records are structurally metadata-only.
    let log = agent_engine::runtime::telemetry::default_trace_log_path()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve the trace log path (no home dir)"))?;
    let stats = trace::export_metadata(&log, &id, &output)?;
    eprintln!(
        "exported {} of {} records to {}",
        stats.exported,
        stats.scanned,
        output.display()
    );
    Ok(())
}
