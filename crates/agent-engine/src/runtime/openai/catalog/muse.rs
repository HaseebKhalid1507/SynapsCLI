//! Meta Muse model catalog and exact-id reasoning capability.
//!
//! Evidence: the official Muse Code CLI (`muse 1.4.6-R6000.1`) reads its
//! model list from `GET https://api.meta.ai/muse-code/models` (Bearer = Model
//! API key). The live response (observed 2026-10) is an OpenAI-style list;
//! each row's capabilities sit under `metadata["muse-code"]`: `is_hidden`,
//! `limit.{context,output}`, `modalities.{input,output}`, client `options`
//! (`reasoningEffort` default, `forceReasoning`, ...) and `variants` — a map
//! of named presets each carrying a `reasoningEffort` drawn from
//! `none|minimal|low|medium|high|xhigh|max|ultra`. Meta's public Model API
//! example uses `muse-wonder-1.0` on `POST /v1/responses`.
//!
//! The catalog is server-driven, so the static table is deliberately tiny:
//! only the publicly documented id ships as a seed, and it carries NO
//! reasoning claim (only the provider default is expressible). Anything else
//! — more ids, exact effort sets, default effort, limits, image input —
//! comes from the live catalog via the capability cache.
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
        ReasoningSupport::NamedEffort { supported, .. } if !supported.is_empty() => Some(supported),
        _ => None,
    }
}

/// The live catalog's default effort for `id` (`options.reasoningEffort`,
/// what the official client sends when the user picks nothing). Only
/// returned when it is a member of the exact advertised set.
pub fn muse_default_level(id: &str) -> Option<ReasoningLevel> {
    match capability_cache::get(&format!("{MUSE_PROVIDER_KEY}/{id}"))?.reasoning {
        ReasoningSupport::NamedEffort {
            supported,
            default_level: Some(level),
        } if supported.contains(&level) => Some(level),
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
    /// Observed wire shape: capabilities live under `metadata["muse-code"]`.
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct MuseEffortVariant {
    #[serde(default)]
    effort: Option<String>,
}

/// `metadata["muse-code"]` as served by `GET /muse-code/models` (models.dev
/// style descriptor plus client `options` / `variants`). Every field is
/// optional; a block that fails to deserialize is ignored as a whole (the row
/// still routes, it just makes no capability claims).
#[derive(Deserialize, Default)]
struct MuseCodeMeta {
    #[serde(default)]
    is_hidden: Option<bool>,
    #[serde(default)]
    limit: Option<MuseLimit>,
    #[serde(default)]
    modalities: Option<MuseModalities>,
    #[serde(default)]
    options: Option<MuseOptions>,
    /// `{"<name>": {"reasoningEffort": "<effort>", ...}, ...}`
    #[serde(default)]
    variants: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize, Default)]
struct MuseLimit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

#[derive(Deserialize, Default)]
struct MuseModalities {
    #[serde(default)]
    input: Option<Vec<String>>,
    #[serde(default)]
    output: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MuseOptions {
    #[serde(default)]
    reasoning_effort: Option<String>,
}

/// Stable display/validation order for named efforts (the wire map's key
/// order is not meaningful).
fn effort_rank(level: ReasoningLevel) -> u8 {
    match level {
        ReasoningLevel::Off => 0,
        ReasoningLevel::Low => 1,
        ReasoningLevel::Medium => 2,
        ReasoningLevel::High => 3,
        ReasoningLevel::XHigh => 4,
        ReasoningLevel::Max => 5,
        ReasoningLevel::Ultra => 6,
        ReasoningLevel::Adaptive | ReasoningLevel::UltraCode => u8::MAX,
    }
}

fn push_unique(supported: &mut Vec<ReasoningLevel>, level: ReasoningLevel) {
    if !supported.contains(&level) {
        supported.push(level);
    }
}

/// Parse `GET /muse-code/models`.
///
/// Observed wire shape (Meta, 2026-10): `{"object":"list","data":[{"id",
/// "object","created","owned_by","metadata":{"muse-code":{"is_hidden",
/// "limit":{"context","output"},"modalities":{"input","output"},
/// "options":{"reasoningEffort",...},"variants":{"<name>":{"reasoningEffort"}},
/// ...}}}]}`. A flat `{"models":[{display_label, reasoning_effort_variants,
/// ...}]}` shape is also accepted. Hidden rows, rows that cannot output text
/// (e.g. image generators) and ids outside the safe shape are dropped; rows
/// that fail to deserialize are skipped individually so one novel row cannot
/// blank the whole catalog. Effort variants become an exact
/// [`ReasoningSupport::NamedEffort`] set; `options.reasoningEffort` becomes
/// its default only when that level is in the set.
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
        let meta: MuseCodeMeta = row
            .metadata
            .as_ref()
            .and_then(|m| m.get("muse-code"))
            .and_then(|m| serde_json::from_value(m.clone()).ok())
            .unwrap_or_default();
        let modalities = meta.modalities.unwrap_or_default();
        let text_output = modalities
            .output
            .as_ref()
            .map_or(true, |out| out.iter().any(|m| m == "text"));
        if row.visibility.as_deref() == Some("hidden")
            || meta.is_hidden == Some(true)
            || !text_output
            || !valid_muse_id(&row.id)
        {
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
        let limit = meta.limit.unwrap_or_default();
        model.context_tokens = row.context_limit.or(limit.context).filter(|n| *n > 0);
        model.max_output_tokens = row.output_limit.or(limit.output).filter(|n| *n > 0);
        if let Some(input) = modalities.input.filter(|i| !i.is_empty()) {
            let mut parsed = vec![Modality::Text];
            for m in input.iter().map(|m| Modality::from_str(m.trim())) {
                if !parsed.contains(&m) {
                    parsed.push(m);
                }
            }
            model.input_modalities = parsed;
        }
        let mut supported = Vec::new();
        for effort in row
            .reasoning_effort_variants
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.effort)
            .filter_map(|e| parse_muse_effort(e.trim()))
        {
            push_unique(&mut supported, effort);
        }
        let mut variant_levels = Vec::new();
        // Only an explicit `reasoningEffort` counts. A preset's NAME may
        // describe speed or a tool profile; treating `max` as an effort
        // would authorize a wire value Meta can reject.
        for variant in meta.variants.unwrap_or_default().into_values() {
            let Some(effort) = variant.get("reasoningEffort").and_then(|e| e.as_str()) else {
                continue;
            };
            if let Some(level) = parse_muse_effort(effort.trim()) {
                push_unique(&mut variant_levels, level);
            }
        }
        variant_levels.sort_by_key(|l| effort_rank(*l));
        for level in variant_levels {
            push_unique(&mut supported, level);
        }
        if !supported.is_empty() {
            let default_level = meta
                .options
                .and_then(|o| o.reasoning_effort)
                .and_then(|e| parse_muse_effort(e.trim()))
                .filter(|level| supported.contains(level));
            model.reasoning = ReasoningSupport::NamedEffort {
                supported,
                default_level,
            };
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
                supported: vec![ReasoningLevel::Low, ReasoningLevel::High],
                default_level: None,
            }
        );
        assert!(parse_muse_catalog_models(r#"{"data":[{"id":"m-1"}]}"#)
            .unwrap()
            .iter()
            .any(|m| m.id == "m-1"));
        assert!(parse_muse_catalog_models("{}").is_err());
        assert!(parse_muse_catalog_models("nope").is_err());
    }

    /// Real `GET /muse-code/models` rows (trimmed, 2026-10): capabilities
    /// live under `metadata["muse-code"]`. Regression for live rows parsing
    /// as `ReasoningSupport::Unknown`, which made every named level fail
    /// with "no advertised effort levels".
    const WIRE: &str = r#"{"object":"list","data":[
        {"id":"muse-wonder-1.0","object":"model","created":1,"owned_by":"meta","metadata":{"muse-code":{
          "attachment":true,"description":null,"family":"avocado","is_hidden":false,
          "limit":{"context":1007997,"output":128000},
          "modalities":{"input":["text","image"],"output":["text"]},
          "name":"muse-wonder-1.0",
          "options":{"forceReasoning":true,"include":["reasoning.encrypted_content"],
                     "reasoningEffort":"high","temperature":0.9,"top_p":0.9},
          "reasoning":true,"tool_call":true,
          "variants":{"high":{"reasoningEffort":"high"},"low":{"reasoningEffort":"low"},
                      "max":{"reasoningEffort":"max"},"medium":{"reasoningEffort":"medium"},
                      "minimal":{"reasoningEffort":"minimal"},
                      "xhigh":{"reasoningEffort":"xhigh","selector":{"description":"d"}}}}}},
        {"id":"muse-image-1.0","object":"model","owned_by":"meta","metadata":{"muse-code":{
          "is_hidden":true,"modalities":{"input":["text","image"],"output":["image"]},
          "options":{},"tool_call":false}}},
        {"id":"muse-visible-imagegen-zz7","object":"model","metadata":{"muse-code":{
          "is_hidden":false,"modalities":{"input":["text"],"output":["image"]}}}},
        {"id":"muse-odd-default-zz8","object":"model","metadata":{"muse-code":{
          "options":{"reasoningEffort":"ultra"},"variants":{"fast":{"reasoningEffort":"low"}}}}},
        {"id":"muse-bad-meta-zz9","object":"model","metadata":{"muse-code":{"limit":"nope"}}}
    ]}"#;

    #[test]
    fn wire_catalog_reads_muse_code_metadata() {
        let models = parse_muse_catalog_models(WIRE).unwrap();
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        // Hidden and image-output rows dropped; a malformed metadata block
        // keeps the row (routable, no claims).
        assert_eq!(
            ids,
            [
                "muse-wonder-1.0",
                "muse-odd-default-zz8",
                "muse-bad-meta-zz9"
            ]
        );
        let wonder = &models[0];
        assert_eq!(wonder.source, CatalogSource::Live);
        assert_eq!(wonder.context_tokens, Some(1_007_997));
        assert_eq!(wonder.max_output_tokens, Some(128_000));
        assert_eq!(
            wonder.input_modalities,
            vec![Modality::Text, Modality::Image]
        );
        // Map key order is irrelevant: canonical order, `minimal` dropped,
        // `options.reasoningEffort` is the default.
        assert_eq!(
            wonder.reasoning,
            ReasoningSupport::NamedEffort {
                supported: vec![
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                    ReasoningLevel::XHigh,
                    ReasoningLevel::Max,
                ],
                default_level: Some(ReasoningLevel::High),
            }
        );
        // Variant name is not the effort; a default outside the set is not
        // claimed.
        assert_eq!(
            models[1].reasoning,
            ReasoningSupport::NamedEffort {
                supported: vec![ReasoningLevel::Low],
                default_level: None,
            }
        );
        let bad = &models[2];
        assert_eq!(bad.reasoning, ReasoningSupport::Unknown);
        assert_eq!(bad.input_modalities, vec![Modality::Text]);
        assert_eq!(bad.context_tokens, None);
    }

    /// A preset's NAME is not an effort. `max` here is a speed/tool preset
    /// with no `reasoningEffort`; counting it would authorize (and send)
    /// `reasoning.effort:"max"`, which Meta can reject with a 400.
    #[test]
    fn variant_name_without_reasoning_effort_is_not_an_effort() {
        let wire = r#"{"object":"list","data":[
            {"id":"muse-preset-zz1","object":"model","metadata":{"muse-code":{
              "variants":{"max":{"speed":"fast"},"high":{"tools":"minimal"},
                          "careful":{"reasoningEffort":"medium"}}}}},
            {"id":"muse-preset-zz2","object":"model","metadata":{"muse-code":{
              "variants":{"high":{},"low":{"temperature":0.1}}}}}
        ]}"#;
        let models = parse_muse_catalog_models(wire).unwrap();
        assert_eq!(
            models[0].reasoning,
            ReasoningSupport::NamedEffort {
                supported: vec![ReasoningLevel::Medium],
                default_level: None,
            }
        );
        assert_eq!(models[1].reasoning, ReasoningSupport::Unknown);
    }

    #[test]
    fn wire_catalog_default_drives_default_level_for_model() {
        use crate::runtime::openai::catalog::validation::{
            default_level_for_model, validate_reasoning_mutation,
        };
        let _guard = test_cache_guard();
        capability_cache::replace_provider(MUSE_PROVIDER_KEY, &[]);
        assert_eq!(
            default_level_for_model("muse/muse-wonder-1.0"),
            Some(ReasoningLevel::Adaptive),
            "no live evidence → provider default"
        );
        capability_cache::replace_provider(
            MUSE_PROVIDER_KEY,
            &parse_muse_catalog_models(WIRE).unwrap(),
        );
        assert_eq!(
            muse_default_level("muse-wonder-1.0"),
            Some(ReasoningLevel::High)
        );
        assert_eq!(
            default_level_for_model("muse/muse-wonder-1.0"),
            Some(ReasoningLevel::High)
        );
        assert!(validate_reasoning_mutation("muse/muse-wonder-1.0", ReasoningLevel::XHigh).is_ok());
        assert!(
            validate_reasoning_mutation("muse/muse-wonder-1.0", ReasoningLevel::Ultra).is_err()
        );
        assert_eq!(muse_default_level("muse-odd-default-zz8"), None);
        assert_eq!(
            default_level_for_model("muse/muse-odd-default-zz8"),
            Some(ReasoningLevel::Adaptive)
        );
        capability_cache::replace_provider(MUSE_PROVIDER_KEY, &[]);
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
