use agent_core::prompt::{
    compile_prompt_stack, AdapterRegistry, ModelFamilyId, ModuleMutability, PromptManifest,
    PromptModule, PromptModuleId, PromptModuleSource, PromptSelectors, QualifiedModelId,
    SelectionContext,
};
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand)]
pub(crate) enum PromptAction {
    /// Validate and compile a prompt manifest without starting the runtime.
    Validate { manifest: PathBuf },
    /// Emit secret-safe prompt stack metadata.
    Inspect {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        model: String,
        #[arg(long)]
        family: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

fn module(id: &str, priority: u16, selectors: PromptSelectors) -> anyhow::Result<PromptModule> {
    Ok(PromptModule::new(
        PromptModuleId::parse(id)?,
        "builtin",
        PromptModuleSource::Builtin,
        priority,
        selectors,
        ModuleMutability::ImmutablePolicy,
        format!("builtin:{id}"),
    )?)
}

fn registry(model: &QualifiedModelId, family: &ModelFamilyId) -> anyhow::Result<AdapterRegistry> {
    AdapterRegistry::new(vec![
        module("kernel.base", 0, PromptSelectors::default())?,
        module(
            "adapter.provider",
            10,
            PromptSelectors::provider(model.provider())?,
        )?,
        module("adapter.model", 20, PromptSelectors::family(family.clone()))?,
    ])
    .map_err(Into::into)
}

fn load(path: &PathBuf) -> anyhow::Result<PromptManifest> {
    let text = std::fs::read_to_string(path)?;
    PromptManifest::parse(&text).map_err(Into::into)
}

pub(crate) fn run(action: PromptAction) -> anyhow::Result<()> {
    match action {
        PromptAction::Validate { manifest } => {
            let manifest = load(&manifest)?;
            manifest.validate_references(["kernel.base", "adapter.provider", "adapter.model"])?;
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
            let model = QualifiedModelId::parse(model)?;
            let family = ModelFamilyId::parse(family.unwrap_or_else(|| model.model().to_owned()))?;
            let context = SelectionContext::new(model.clone(), family.clone())?;
            let manifest = load(&manifest)?;
            manifest.validate_references(["kernel.base", "adapter.provider", "adapter.model"])?;
            let registry = registry(&model, &family)?;
            let stack = compile_prompt_stack(&manifest, &registry, &context, None)?;
            let mut inspection = serde_json::to_value(stack.inspect())?;
            inspection["model"] = serde_json::Value::String(model.as_str().to_owned());
            println!("{}", serde_json::to_string_pretty(&inspection)?);
        }
    }
    Ok(())
}
