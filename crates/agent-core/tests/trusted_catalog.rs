use agent_core::orchestration::{CatalogEntry, CatalogSnapshot};
use agent_core::prompt::{PromptManifest, QualifiedModelId};

fn entry(model: &str, available: bool, worker_eligible: bool) -> CatalogEntry {
    CatalogEntry {
        model: QualifiedModelId::parse(model).unwrap(),
        available,
        worker_eligible,
    }
}

#[test]
fn manifest_cannot_manufacture_catalog_authority() {
    let manifest = PromptManifest::parse(
        "schema: synaps-prompt/1\nkernel: k\npolicies:\n  delegation:\n    mode: enforced\n    allowed_models: [anthropic/invented]\n    max_concurrent_workers: 1\n    max_total_workers: 1\n",
    )
    .unwrap();
    let foreground = QualifiedModelId::parse("anthropic/foreground").unwrap();
    let catalog = CatalogSnapshot::from_entries([
        entry("anthropic/foreground", true, true),
        entry("anthropic/invented", false, true),
    ]);
    assert!(manifest.delegation_policy(foreground, &catalog).is_err());
}

#[test]
fn catalog_requires_availability_and_worker_eligibility() {
    for candidate in [
        entry("anthropic/not-available", false, true),
        entry("anthropic/not-worker", true, false),
    ] {
        let model = candidate.model.clone();
        let model_name = model.as_str();
        let manifest = PromptManifest::parse(&format!(
            "schema: synaps-prompt/1\nkernel: k\npolicies:\n  delegation:\n    mode: enforced\n    allowed_models: [{model_name}]\n    max_concurrent_workers: 1\n    max_total_workers: 1\n"
        ))
        .unwrap();
        let foreground = QualifiedModelId::parse("anthropic/foreground").unwrap();
        let catalog =
            CatalogSnapshot::from_entries([entry("anthropic/foreground", true, true), candidate]);
        assert!(manifest.delegation_policy(foreground, &catalog).is_err());
    }
}
