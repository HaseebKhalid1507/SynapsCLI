//! Meta Muse model catalog and exact-id reasoning capability.
//!
//! Evidence: the official Muse Code CLI (`muse 1.4.6-R6000.1`) reads its
//! model list from `GET https://api.meta.ai/muse-code/models` (Bearer = Model
//! API key). Rows carry `id`, `display_label`, `visibility`, `display_order`,
//! `is_default`, `context_limit`, `output_limit` and
//! `reasoning_effort_variants[].effort`, with efforts drawn from
//! `none|minimal|low|medium|high|xhigh|max|ultra`. Meta's public Model API
//! example uses `muse-wonder-1.0` on `POST /v1/responses`.
//!
//! The catalog is server-driven, so the static table is deliberately tiny:
//! only the publicly documented id ships as a seed, and it carries NO
//! reasoning claim (only the provider default is expressible). Anything else
//! — more ids, exact effort sets, limits — comes from the live catalog via
//! the capability cache.
//!
//! Routing follows the registry precedent for server-catalog providers
//! (e.g. `inferx/<any published id>`): any id of the safe shape routes to
//! the pinned `api.meta.ai` origin and Meta is authoritative for existence.
//! Routing must NOT depend on the process-local capability cache — the
//! daemon, a resumed session or a fresh process would otherwise refuse a
//! model the picker (another process) had listed. Capability claims stay
//! evidence-gated: named reasoning levels require the live effort set, and
//! strict validators (subagent authorization, session drivers) still require
//! a seed or an exact live row ([`muse_runtime_model`]).

use agent_core::reasoning::ReasoningLevel;
use serde::Deserialize;

use super::{
    capability_cache, CatalogModel, CatalogProviderKind, CatalogSource, Modality, PricingSummary,
    ReasoningSupport,
};

/// Canonical provider key / model prefix (`muse/<id>`).
pub const MUSE_PROVIDER_KEY: &str = "muse";
/// Human-readable provider name.
pub const MUSE_PROVIDER_NAME: &str = "Meta Muse";
/// Broker path for the live catalog (pinned origin `https://api.meta.ai`).
pub const MUSE_MODELS_PATH: &str = "/muse-code/models";

/// One statically known Muse model row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuseModelDescriptor {
    pub id: &'static str,
    pub label: &'static str,
}

/// Publicly documented ids only (Meta Model API example request).
pub const MUSE_TEXT_MODELS: &[MuseModelDescriptor] = &[MuseModelDescriptor {
    id: "muse-wonder-1.0",
    label: "Muse Wonder 1.0",
}];

/// Exact static lookup — never substring-based.
pub fn muse_static_model(id: &str) -> Option<&'static MuseModelDescriptor> {
    MUSE_TEXT_MODELS.iter().find(|model| model.id == id)
}

/// Conservative id shape for live rows: lowercase ascii alnum plus `.-_`,
/// starting alphanumeric, bounded length. Anything else is dropped.
fn valid_muse_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
        })
}

/// Serializes tests that replace the process-global `muse/*` cache rows
/// (`capability_cache::replace_provider` is provider-wide, so concurrent
/// tests would clobber each other's live rows).
#[cfg(test)]
pub(crate) static TEST_CACHE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn test_cache_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_CACHE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// True when `id` may be routed to the pinned Muse origin: the safe id
/// shape only. Existence is Meta's decision (a typo surfaces as an upstream
/// error on the first turn, exactly like other server-catalog providers).
pub fn muse_routable_id(id: &str) -> bool {
    valid_muse_id(id)
}

/// True when `id` is KNOWN: a static seed, or an exact row the live Muse
/// catalog put in the capability cache this process. Strict validators use
/// this; routing uses [`muse_routable_id`].
pub fn muse_runtime_model(id: &str) -> bool {
    muse_static_model(id).is_some()
        || capability_cache::get(&format!("{MUSE_PROVIDER_KEY}/{id}"))
            .is_some_and(|row| row.provider_key == MUSE_PROVIDER_KEY && row.id == id)
}

/// Exact named effort set for a routable Muse id. `None` = no authoritative
/// evidence (static seed, or a live row that advertised no variants): only
/// the provider default (`adaptive`, field omitted) may be sent.
pub fn muse_supported_levels(id: &str) -> Option<Vec<ReasoningLevel>> {
    match capability_cache::get(&format!("{MUSE_PROVIDER_KEY}/{id}"))?.reasoning {
        ReasoningSupport::NamedEffort { supported } if !supported.is_empty() => Some(supported),
        _ => None,
    }
}

/// Wire label for a validated level. `Adaptive` (and levels Muse has no name
/// for) → `None` → the `reasoning` field is omitted (provider default).
pub fn muse_effort_label(level: ReasoningLevel) -> Option<&'static str> {
    match level {
        ReasoningLevel::Off => Some("none"),
        ReasoningLevel::Low => Some("low"),
        ReasoningLevel::Medium => Some("medium"),
        ReasoningLevel::High => Some("high"),
        ReasoningLevel::XHigh => Some("xhigh"),
        ReasoningLevel::Max => Some("max"),
        ReasoningLevel::Ultra => Some("ultra"),
        ReasoningLevel::Adaptive | ReasoningLevel::UltraCode => None,
    }
}

/// Inverse of [`muse_effort_label`] for catalog parsing. `minimal` has no
/// Synaps level and is dropped rather than approximated.
fn parse_muse_effort(effort: &str) -> Option<ReasoningLevel> {
    match effort {
        "none" => Some(ReasoningLevel::Off),
        "low" => Some(ReasoningLevel::Low),
        "medium" => Some(ReasoningLevel::Medium),
        "high" => Some(ReasoningLevel::High),
        "xhigh" => Some(ReasoningLevel::XHigh),
        "max" => Some(ReasoningLevel::Max),
        "ultra" => Some(ReasoningLevel::Ultra),
        _ => None,
    }
}

fn muse_row(id: &str, label: Option<String>) -> CatalogModel {
    CatalogModel {
        provider_key: MUSE_PROVIDER_KEY.into(),
        provider_name: MUSE_PROVIDER_NAME.into(),
        provider_kind: CatalogProviderKind::Generic {
            key: MUSE_PROVIDER_KEY.into(),
        },
        id: id.into(),
        label,
        context_tokens: None,
        max_output_tokens: None,
        input_modalities: vec![Modality::Text],
        pricing: PricingSummary::default(),
        reasoning: ReasoningSupport::Unknown,
        source: CatalogSource::Live,
    }
}

/// Static seeds for pickers and the not-logged-in fallback.
pub fn muse_static_catalog_models() -> Vec<CatalogModel> {
    MUSE_TEXT_MODELS
        .iter()
        .map(|descriptor| {
            let mut row = muse_row(descriptor.id, Some(descriptor.label.into()));
            row.source = CatalogSource::StaticFallback;
            row
        })
        .collect()
}

#[derive(Deserialize)]
struct MuseCatalogBody {
    #[serde(default)]
    models: Option<Vec<serde_json::Value>>,
    /// OpenAI-style `{"data":[...]}` tolerated as a fallback shape.
    #[serde(default)]
    data: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct MuseCatalogRow {
    id: String,
    #[serde(default)]
    display_label: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    display_order: Option<i64>,
    #[serde(default)]
    context_limit: Option<u64>,
    #[serde(default)]
    output_limit: Option<u64>,
    #[serde(default)]
    reasoning_effort_variants: Option<Vec<MuseEffortVariant>>,
}

#[derive(Deserialize)]
struct MuseEffortVariant {
    #[serde(default)]
    effort: Option<String>,
}

/// Parse `GET /muse-code/models`. Hidden rows and ids outside the safe shape
/// are dropped; rows that fail to deserialize are skipped individually so one
/// novel row cannot blank the whole catalog. Effort variants become an exact
/// [`ReasoningSupport::NamedEffort`] set in catalog order.
pub fn parse_muse_catalog_models(body: &str) -> Result<Vec<CatalogModel>, String> {
    let parsed: MuseCatalogBody =
        serde_json::from_str(body).map_err(|e| format!("invalid Muse catalog JSON: {e}"))?;
    let rows = parsed
        .models
        .or(parsed.data)
        .ok_or_else(|| "Muse catalog has no models array".to_string())?;
    let mut ordered: Vec<(i64, usize, CatalogModel)> = Vec::new();
    for (index, value) in rows.into_iter().enumerate() {
        let Ok(row) = serde_json::from_value::<MuseCatalogRow>(value) else {
            continue;
        };
        if row.visibility.as_deref() == Some("hidden") || !valid_muse_id(&row.id) {
            continue;
        }
        if ordered.iter().any(|(_, _, existing)| existing.id == row.id) {
            continue;
        }
        let label = row
            .display_label
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty());
        let mut model = muse_row(&row.id, label);
        model.context_tokens = row.context_limit.filter(|n| *n > 0);
        model.max_output_tokens = row.output_limit.filter(|n| *n > 0);
        let mut supported = Vec::new();
        for effort in row
            .reasoning_effort_variants
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.effort)
            .filter_map(|e| parse_muse_effort(e.trim()))
        {
            if !supported.contains(&effort) {
                supported.push(effort);
            }
        }
        if !supported.is_empty() {
            model.reasoning = ReasoningSupport::NamedEffort { supported };
        }
        ordered.push((row.display_order.unwrap_or(i64::MAX), index, model));
    }
    ordered.sort_by_key(|(order, index, _)| (*order, *index));
    Ok(ordered.into_iter().map(|(_, _, model)| model).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = r#"{"schema_version":1,"provider_id":"meta","source":"network","models":[
        {"id":"muse-spark-1.3","display_label":"Muse Spark 1.3","visibility":"visible",
         "display_order":2,"is_default":false,"context_limit":262144,"output_limit":128000,
         "reasoning_effort_variants":[{"effort":"low","description":"l"},{"effort":"minimal"},
           {"effort":"high"},{"effort":"high"},{"effort":"bogus"}]},
        {"id":"muse-wonder-1.0","display_label":"Muse Wonder 1.0","visibility":"visible",
         "display_order":1,"is_default":true,"context_limit":1000000,"output_limit":0,
         "reasoning_effort_variants":[]},
        {"id":"muse-internal-x","visibility":"hidden","display_order":0},
        {"id":"Bad Id/../x","display_order":0},
        {"display_label":"no id"},
        {"id":"muse-novel-row","display_order":"not-a-number"}
    ]}"#;

    #[test]
    fn effort_labels_round_trip_and_adaptive_omits() {
        for level in [
            ReasoningLevel::Off,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
            ReasoningLevel::Ultra,
        ] {
            let label = muse_effort_label(level).unwrap();
            assert_eq!(parse_muse_effort(label), Some(level), "{label}");
        }
        assert_eq!(muse_effort_label(ReasoningLevel::Adaptive), None);
        assert_eq!(muse_effort_label(ReasoningLevel::UltraCode), None);
        assert_eq!(parse_muse_effort("minimal"), None);
    }

    #[test]
    fn live_catalog_parse_orders_filters_and_reads_exact_efforts() {
        let models = parse_muse_catalog_models(LIVE).unwrap();
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        // display_order sort; hidden, malformed and id-less rows dropped; a
        // row with a bad field type is skipped without blanking the catalog.
        assert_eq!(ids, ["muse-wonder-1.0", "muse-spark-1.3"]);
        let wonder = &models[0];
        assert_eq!(wonder.runtime_id(), "muse/muse-wonder-1.0");
        assert_eq!(wonder.context_tokens, Some(1_000_000));
        assert_eq!(wonder.max_output_tokens, None, "0 is not a limit");
        assert_eq!(wonder.reasoning, ReasoningSupport::Unknown);
        let spark = &models[1];
        assert_eq!(spark.label.as_deref(), Some("Muse Spark 1.3"));
        assert_eq!(
            spark.reasoning,
            ReasoningSupport::NamedEffort {
                supported: vec![ReasoningLevel::Low, ReasoningLevel::High]
            }
        );
        assert!(parse_muse_catalog_models(r#"{"data":[{"id":"m-1"}]}"#)
            .unwrap()
            .iter()
            .any(|m| m.id == "m-1"));
        assert!(parse_muse_catalog_models("{}").is_err());
        assert!(parse_muse_catalog_models("nope").is_err());
    }

    #[test]
    fn static_seed_is_documented_id_only_and_ids_fail_closed() {
        assert!(muse_static_model("muse-wonder-1.0").is_some());
        assert!(muse_static_model("muse-wonder").is_none());
        let seeds = muse_static_catalog_models();
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].source, CatalogSource::StaticFallback);
        assert_eq!(seeds[0].reasoning, ReasoningSupport::Unknown);
        assert!(muse_runtime_model("muse-wonder-1.0"));
        // Known (strict) vs routable (shape): uncataloged ids route but are
        // not "known" to strict validators.
        assert!(!muse_runtime_model("never-cataloged-zz9"));
        assert!(muse_routable_id("never-cataloged-zz9"));
        assert!(!muse_routable_id("Bad Id"));
        assert_eq!(muse_supported_levels("muse-wonder-1.0"), None);
    }

    #[test]
    fn id_shape_guard() {
        for ok in ["muse-wonder-1.0", "muse_spark.1", "a"] {
            assert!(valid_muse_id(ok), "{ok}");
        }
        for bad in ["", "Muse", "-x", "a/b", "a b", "../x", &"a".repeat(129)] {
            assert!(!valid_muse_id(bad), "{bad}");
        }
    }
}
