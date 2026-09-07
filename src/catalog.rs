use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use codex_protocol::openai_models::{InputModality, ModelInfo, ModelVisibility, ReasoningEffort};
use serde::{Deserialize, Serialize};

use crate::error::{BridgeError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelVariant {
    pub gateway_id: String,
    pub model_slug: String,
    pub display_name: String,
    pub description: Option<String>,
    pub context_window: i64,
    pub usable_tokens: i64,
    pub auto_compact_tokens: i64,
    pub supports_images: bool,
    pub supported_efforts: Vec<ReasoningEffort>,
    pub default_effort: ReasoningEffort,
    #[serde(default)]
    pub multi_agent_reasoning_effort: Option<ReasoningEffort>,
    pub extended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    variants: Vec<ModelVariant>,
}

impl Catalog {
    pub fn from_codex(mut models: Vec<ModelInfo>) -> Result<Self> {
        models.retain(is_compatible_gpt_model);
        models.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.slug.cmp(&right.slug))
        });

        let mut variants = Vec::new();
        for model in models {
            let standard_window = model.context_window.unwrap_or_default();
            let extended_window = model.max_context_window.unwrap_or(standard_window);

            if extended_window > standard_window {
                variants.push(model_variant(&model, extended_window, true));
            }
            variants.push(model_variant(&model, standard_window, false));
        }

        if variants.is_empty() {
            return Err(BridgeError::InvalidCatalog(
                "no visible subscription GPT models with text input and a positive context window"
                    .to_string(),
            ));
        }

        Ok(Self { variants })
    }

    pub fn default(&self) -> &ModelVariant {
        self.variants
            .iter()
            .find(|variant| variant.model_slug == "gpt-5.6-terra" && variant.extended)
            .or_else(|| {
                self.variants
                    .iter()
                    .find(|variant| variant.model_slug == "gpt-5.6-terra")
            })
            .unwrap_or(&self.variants[0])
    }

    pub fn visible(&self) -> &[ModelVariant] {
        &self.variants
    }

    pub fn resolve(&self, gateway_id: &str) -> Result<&ModelVariant> {
        self.variants
            .iter()
            .find(|variant| {
                variant.gateway_id == gateway_id
                    || variant.gateway_id.strip_suffix("[1m]") == Some(gateway_id)
            })
            .ok_or_else(|| BridgeError::UnknownModel(gateway_id.to_string()))
    }

    pub fn resolve_for_input(
        &self,
        gateway_id: &str,
        estimated_input_tokens: u64,
    ) -> Result<&ModelVariant> {
        let selected = self.resolve(gateway_id)?;
        if estimated_input_tokens <= u64::try_from(selected.usable_tokens).unwrap_or_default() {
            return Ok(selected);
        }

        Ok(self
            .variants
            .iter()
            .filter(|candidate| {
                candidate.model_slug == selected.model_slug
                    && candidate.extended
                    && candidate.usable_tokens > selected.usable_tokens
                    && estimated_input_tokens
                        <= u64::try_from(candidate.usable_tokens).unwrap_or_default()
            })
            .max_by_key(|candidate| candidate.usable_tokens)
            .unwrap_or(selected))
    }
}

pub fn gateway_id(slug: &str, tokens: i64) -> String {
    format!("claude-gpt-openai::{slug}::{tokens}")
}

pub fn usable_tokens(window: i64, percent: i64) -> i64 {
    window.saturating_mul(percent).saturating_div(100)
}

pub fn save_cached_catalog(path: &Path, catalog: &Catalog) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        BridgeError::InvalidCatalog(format!("cache path has no parent: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(|source| BridgeError::Write {
        path: parent.to_path_buf(),
        source,
    })?;

    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| BridgeError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    serde_json::to_writer_pretty(&mut temporary, catalog).map_err(|source| {
        BridgeError::SerializeJson {
            path: path.to_path_buf(),
            source,
        }
    })?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| BridgeError::Write {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| BridgeError::Write {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

pub fn load_cached_catalog(path: &Path) -> Result<Catalog> {
    let file = File::open(path).map_err(|source| BridgeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let catalog =
        serde_json::from_reader(BufReader::new(file)).map_err(|source| BridgeError::ParseJson {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(catalog)
}

fn is_compatible_gpt_model(model: &ModelInfo) -> bool {
    model.visibility == ModelVisibility::List
        && model.slug.starts_with("gpt-")
        && model.input_modalities.contains(&InputModality::Text)
        && model.context_window.is_some_and(|window| window > 0)
}

fn model_variant(model: &ModelInfo, context_window: i64, extended: bool) -> ModelVariant {
    let supported_efforts = model
        .supported_reasoning_levels
        .iter()
        .map(|preset| preset.effort.clone())
        .collect::<Vec<_>>();
    let default_effort = model
        .default_reasoning_level
        .clone()
        .filter(|effort| supported_efforts.contains(effort))
        .or_else(|| supported_efforts.first().cloned())
        .unwrap_or_default();
    let compact_ceiling = context_window.saturating_mul(9).saturating_div(10);
    let auto_compact_tokens = model
        .auto_compact_token_limit
        .map_or(compact_ceiling, |limit| limit.min(compact_ceiling));
    let gateway_id = if extended {
        format!("{}[1m]", gateway_id(&model.slug, context_window))
    } else {
        gateway_id(&model.slug, context_window)
    };

    ModelVariant {
        gateway_id,
        model_slug: model.slug.clone(),
        display_name: format!("{} [{}]", model.display_name, format_window(context_window)),
        description: model.description.clone(),
        context_window,
        usable_tokens: usable_tokens(context_window, model.effective_context_window_percent),
        auto_compact_tokens,
        supports_images: model.input_modalities.contains(&InputModality::Image),
        supported_efforts,
        default_effort,
        multi_agent_reasoning_effort: model.multi_agent_reasoning_effort.clone(),
        extended,
    }
}

fn format_window(tokens: i64) -> String {
    if tokens % 1_000_000 == 0 {
        format!("{}M", tokens / 1_000_000)
    } else if tokens % 1_000 == 0 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}
