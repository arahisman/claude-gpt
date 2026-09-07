use codex_protocol::openai_models::ReasoningEffort;
use serde::{Deserialize, Serialize};

pub type CodexEffort = ReasoningEffort;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortResolution {
    pub requested: ClaudeEffort,
    pub actual: CodexEffort,
    pub fell_back: bool,
}

pub fn resolve_effort(
    requested: ClaudeEffort,
    supported: &[CodexEffort],
    default: CodexEffort,
) -> EffortResolution {
    let candidates = match requested {
        ClaudeEffort::Low => vec![CodexEffort::Low],
        ClaudeEffort::Medium => vec![CodexEffort::Medium, CodexEffort::Low],
        ClaudeEffort::High => vec![CodexEffort::High, CodexEffort::Medium, CodexEffort::Low],
        ClaudeEffort::XHigh => vec![
            CodexEffort::XHigh,
            CodexEffort::High,
            CodexEffort::Medium,
            CodexEffort::Low,
        ],
        ClaudeEffort::Max => vec![
            CodexEffort::Ultra,
            CodexEffort::Max,
            CodexEffort::XHigh,
            CodexEffort::High,
            CodexEffort::Medium,
            CodexEffort::Low,
        ],
    };
    let direct = candidates[0].clone();
    let actual = candidates
        .into_iter()
        .find(|candidate| supported.contains(candidate))
        .or_else(|| supported.last().cloned())
        .unwrap_or(default);

    EffortResolution {
        requested,
        fell_back: actual != direct,
        actual,
    }
}

pub fn wire_effort(
    semantic: &CodexEffort,
    supported: &[CodexEffort],
    multi_agent: Option<&CodexEffort>,
) -> CodexEffort {
    if semantic != &CodexEffort::Ultra {
        return semantic.clone();
    }
    multi_agent
        .filter(|effort| {
            *effort != &CodexEffort::Ultra && supported.iter().any(|value| value == *effort)
        })
        .cloned()
        .or_else(|| {
            supported
                .iter()
                .find(|effort| effort == &&CodexEffort::Max)
                .cloned()
        })
        .or_else(|| {
            supported
                .iter()
                .rev()
                .find(|effort| effort != &&CodexEffort::Ultra)
                .cloned()
        })
        .unwrap_or(CodexEffort::Medium)
}
