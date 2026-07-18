use agent_core::prompt::{
    compile_prompt_stack, ModelFamilyId, PromptManifest, QualifiedModelId, SelectionContext,
};
use clap::Subcommand;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub(crate) enum PromptAction {
    /// Validate and compile a prompt manifest without starting the runtime.
    Validate {
        manifest: PathBuf,
        #[arg(long, value_name = "QUALIFIED_MODEL")]
        model: String,
        #[arg(long)]
        family: Option<String>,
    },
    /// Emit secret-safe prompt stack metadata.
    Inspect {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long, value_name = "QUALIFIED_MODEL")]
        model: String,
        #[arg(long)]
        family: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

fn load(path: &Path) -> anyhow::Result<PromptManifest> {
    PromptManifest::parse(&std::fs::read_to_string(path)?).map_err(Into::into)
}
fn context(model: String, family: Option<String>) -> anyhow::Result<SelectionContext> {
    SelectionContext::new(
        QualifiedModelId::parse(model)?,
        family.map(ModelFamilyId::parse).transpose()?,
    )
    .map_err(Into::into)
}
fn compile(
    path: &Path,
    model: String,
    family: Option<String>,
) -> anyhow::Result<agent_core::prompt::PromptStack> {
    let context = context(model, family)?;
    let manifest = load(path)?;
    let registry = manifest.registry(path.parent())?;
    compile_prompt_stack(&manifest, &registry, &context, None).map_err(Into::into)
}
pub(crate) fn run(action: PromptAction) -> anyhow::Result<()> {
    match action {
        PromptAction::Validate {
            manifest,
            model,
            family,
        } => {
            compile(&manifest, model, family)?;
        }
        PromptAction::Inspect {
            manifest,
            model,
            family,
            json,
        } => {
            if !json {
                anyhow::bail!("inspect currently requires --json");
            }
            let foreground = QualifiedModelId::parse(model.clone())?;
            let loaded = load(&manifest)?;
            let catalog = agent_engine::orchestration::OrchestrationRuntime::trusted_catalog(
                &foreground,
                loaded.delegation_catalog_candidates(),
            )
            .map_err(|error| anyhow::anyhow!(error))?;
            let mode = loaded
                .delegation_policy(foreground, &catalog)?
                .map(|policy| policy.mode)
                .unwrap_or(agent_core::orchestration::EnforcementMode::Off);
            let stack = compile(&manifest, model, family)?;
            println!("{}", serde_json::to_string_pretty(&stack.inspect(mode))?);
        }
    }
    Ok(())
}
