use agent_core::orchestration::{
    CatalogSnapshot, CrossProviderGrant, DelegationPolicy, DispatchFailureCode,
};
use agent_core::prompt::QualifiedModelId;

fn model(value: &str) -> QualifiedModelId {
    QualifiedModelId::parse(value).unwrap()
}

#[test]
fn exact_qualified_identity_and_catalog_authority() {
    let foreground = model("openai-codex/gpt-5.6-sol");
    assert_eq!(foreground.provider(), "openai-codex");
    assert_eq!(
        model("openrouter/vendor/model/v2").model(),
        "vendor/model/v2"
    );
    assert_ne!(
        foreground.provider(),
        model("openai/gpt-5.6-sol").provider()
    );

    let catalog = CatalogSnapshot::new([foreground.clone(), model("openai-codex/gpt-5.6-fast")]);
    let policy = DelegationPolicy::baseline(foreground.clone(), catalog.clone(), 2, 4).unwrap();
    assert_eq!(policy.effective_choices(), &[foreground.clone()]);
    assert_eq!(policy.catalog_snapshot_id(), catalog.id());
    assert_eq!(
        policy
            .authorize(&model("openai-codex/invented"))
            .unwrap_err()
            .typed_code(),
        DispatchFailureCode::CatalogModelUnknown
    );
}

#[test]
fn exact_cross_provider_grants_never_imply_provider_wide_authority() {
    let foreground = model("openai-codex/gpt-5.6-sol");
    let granted = model("anthropic/claude-opus-4-7");
    let ungranted = model("anthropic/claude-sonnet-4-6");
    let catalog = CatalogSnapshot::new([foreground.clone(), granted.clone(), ungranted.clone()]);
    let policy = DelegationPolicy::with_grants(
        foreground.clone(),
        catalog,
        [foreground],
        [
            CrossProviderGrant::new("review-01", "openai-codex", "anthropic", [granted.clone()])
                .unwrap(),
        ],
        2,
        4,
    )
    .unwrap();
    assert!(policy.authorize(&granted).is_ok());
    assert_eq!(
        policy.authorize(&ungranted).unwrap_err().typed_code(),
        DispatchFailureCode::ModelNotAllowed
    );
}

#[test]
fn catalog_snapshot_and_choices_are_deterministic() {
    let a = CatalogSnapshot::new([model("z/model"), model("a/model"), model("a/model")]);
    let b = CatalogSnapshot::new([model("a/model"), model("z/model")]);
    assert_eq!(a.id(), b.id());
    assert_eq!(a.digest_sha256(), b.digest_sha256());
}
