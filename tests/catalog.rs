use claude_gpt::catalog::{Catalog, load_cached_catalog, save_cached_catalog};
use claude_gpt::effort::{ClaudeEffort, CodexEffort, resolve_effort, wire_effort};
use codex_protocol::openai_models::ModelsResponse;

fn fixture_catalog() -> Catalog {
    let response: ModelsResponse =
        serde_json::from_str(include_str!("fixtures/models.json")).expect("valid Codex fixture");
    Catalog::from_codex(response.models).expect("compatible GPT catalog")
}

#[test]
fn creates_standard_and_extended_variants_and_prefers_extended_terra() {
    let catalog = fixture_catalog();

    assert_eq!(catalog.default().display_name, "GPT-5.6-Terra [872K]");
    assert_eq!(
        catalog
            .visible()
            .iter()
            .filter(|model| model.model_slug == "gpt-5.6-sol")
            .count(),
        2
    );
    assert_eq!(catalog.default().context_window, 872_000);
    assert_eq!(catalog.default().usable_tokens, 828_400);
    assert_eq!(
        catalog.default().gateway_id,
        "claude-gpt-openai::gpt-5.6-terra::872000[1m]"
    );
}

#[test]
fn filters_hidden_and_service_models() {
    let catalog = fixture_catalog();
    let slugs = catalog
        .visible()
        .iter()
        .map(|model| model.model_slug.as_str())
        .collect::<Vec<_>>();

    assert!(!slugs.contains(&"codex-auto-review"));
    assert!(!slugs.contains(&"gpt-hidden"));
    assert!(slugs.contains(&"gpt-5.6-sol"));
    assert!(slugs.contains(&"gpt-5.5"));
}

#[test]
fn includes_chatgpt_only_spark_with_its_live_limits() {
    let catalog = fixture_catalog();
    let spark = catalog
        .visible()
        .iter()
        .find(|model| model.model_slug == "gpt-5.3-codex-spark")
        .expect("subscription-only Spark model");

    assert_eq!(spark.display_name, "GPT-5.3-Codex-Spark [128K]");
    assert_eq!(spark.context_window, 128_000);
    assert_eq!(spark.usable_tokens, 121_600);
    assert_eq!(spark.default_effort, CodexEffort::High);
    assert_eq!(
        resolve_effort(
            ClaudeEffort::Max,
            &spark.supported_efforts,
            spark.default_effort.clone(),
        )
        .actual,
        CodexEffort::XHigh
    );
}

#[test]
fn resolves_stable_gateway_ids_without_model_substitution() {
    let catalog = fixture_catalog();
    let id = "claude-gpt-openai::gpt-5.6-sol::272000";

    assert_eq!(
        catalog.resolve(id).expect("known ID").model_slug,
        "gpt-5.6-sol"
    );
    assert_eq!(
        catalog
            .resolve("claude-gpt-openai::gpt-5.6-sol::872000")
            .expect("Claude strips the local 1m marker before dispatch")
            .context_window,
        872_000
    );
    assert!(
        catalog
            .resolve("claude-gpt-openai::missing::272000")
            .is_err()
    );
}

#[test]
fn promotes_only_an_overflowing_resumed_window_to_the_same_models_extended_variant() {
    let catalog = fixture_catalog();
    let resumed_standard_id = "claude-gpt-openai::gpt-5.6-terra::272000";

    assert_eq!(
        catalog
            .resolve_for_input(resumed_standard_id, 258_400)
            .expect("the standard usable window still fits")
            .context_window,
        272_000
    );
    assert_eq!(
        catalog
            .resolve_for_input(resumed_standard_id, 258_401)
            .expect("the extended variant fits the overflow")
            .context_window,
        872_000
    );
}

#[test]
fn maps_claude_max_to_the_highest_live_effort() {
    let all = [
        CodexEffort::Low,
        CodexEffort::Medium,
        CodexEffort::High,
        CodexEffort::XHigh,
        CodexEffort::Max,
        CodexEffort::Ultra,
    ];
    let without_ultra = &all[..5];
    let through_xhigh = &all[..4];

    assert_eq!(
        resolve_effort(ClaudeEffort::Max, &all, CodexEffort::Low).actual,
        CodexEffort::Ultra
    );
    assert_eq!(
        resolve_effort(ClaudeEffort::Max, without_ultra, CodexEffort::Low).actual,
        CodexEffort::Max
    );
    assert_eq!(
        resolve_effort(ClaudeEffort::Max, through_xhigh, CodexEffort::Low).actual,
        CodexEffort::XHigh
    );
}

#[test]
fn encodes_ultra_with_the_catalog_multi_agent_wire_effort() {
    let supported = [
        CodexEffort::Low,
        CodexEffort::XHigh,
        CodexEffort::Max,
        CodexEffort::Ultra,
    ];

    assert_eq!(
        wire_effort(&CodexEffort::Ultra, &supported, Some(&CodexEffort::XHigh)),
        CodexEffort::XHigh
    );
    assert_eq!(
        wire_effort(&CodexEffort::Ultra, &supported, None),
        CodexEffort::Max
    );
}

#[test]
fn falls_back_downward_for_an_unsupported_non_max_effort() {
    let resolution = resolve_effort(
        ClaudeEffort::XHigh,
        &[CodexEffort::Low, CodexEffort::Medium, CodexEffort::High],
        CodexEffort::Medium,
    );

    assert_eq!(resolution.actual, CodexEffort::High);
    assert!(resolution.fell_back);
}

#[test]
fn saves_and_loads_the_last_known_good_catalog_atomically() {
    let directory = tempfile::tempdir().expect("temporary cache directory");
    let path = directory.path().join("models-cache.json");
    let catalog = fixture_catalog();

    save_cached_catalog(&path, &catalog).expect("save cache");
    let loaded = load_cached_catalog(&path).expect("load cache");

    assert_eq!(loaded, catalog);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}
