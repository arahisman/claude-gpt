use base64::Engine;
use claude_gpt::anthropic::{AnthropicRequest, ContentBlock, Message};
use claude_gpt::catalog::ModelVariant;
use claude_gpt::effort::{ClaudeEffort, CodexEffort, EffortResolution};
use claude_gpt::responses::{convert_request, estimate_request_tokens};
use serde_json::{Value, json};

fn variant(context_window: i64, usable_tokens: i64, supports_images: bool) -> ModelVariant {
    ModelVariant {
        gateway_id: format!("claude-gpt-openai::gpt-5.6-sol::{context_window}"),
        model_slug: "gpt-5.6-sol".to_string(),
        display_name: "GPT-5.6-Sol".to_string(),
        description: None,
        context_window,
        usable_tokens,
        auto_compact_tokens: usable_tokens.saturating_mul(9) / 10,
        supports_images,
        supported_efforts: vec![CodexEffort::XHigh, CodexEffort::Ultra],
        default_effort: CodexEffort::Ultra,
        multi_agent_reasoning_effort: Some(CodexEffort::XHigh),
        extended: true,
    }
}

fn ultra() -> EffortResolution {
    EffortResolution {
        requested: ClaudeEffort::Max,
        actual: CodexEffort::Ultra,
        fell_back: false,
    }
}

#[test]
fn converts_mixed_history_images_and_parallel_tools() {
    let request: AnthropicRequest =
        serde_json::from_str(include_str!("fixtures/anthropic_mixed_request.json"))
            .expect("valid Anthropic fixture");
    let actual = convert_request(&request, &variant(872_000, 828_400, true), ultra())
        .expect("convert request");
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/responses_mixed_request.json"))
            .expect("valid Responses fixture");

    assert_eq!(actual, expected);
}

#[test]
fn rejects_orphan_tool_results_with_a_json_path() {
    let request = AnthropicRequest {
        model: "claude-gpt-openai::gpt-5.6-sol::872000".to_string(),
        max_tokens: 1024,
        system: Default::default(),
        messages: vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "missing".to_string(),
                content: Value::String("done".to_string()),
                is_error: false,
            }],
        }],
        tools: Vec::new(),
        tool_choice: None,
        stream: true,
        stop_sequences: Vec::new(),
    };

    let error = convert_request(&request, &variant(872_000, 828_400, true), ultra())
        .expect_err("orphan result must fail");

    assert!(
        error
            .to_string()
            .contains("messages[0].content[0].tool_use_id")
    );
}

#[test]
fn preserves_claude_tool_search_references_in_tool_results() {
    let request = AnthropicRequest {
        model: "claude-gpt-openai::gpt-5.6-sol::872000".to_string(),
        max_tokens: 1024,
        system: Default::default(),
        messages: vec![
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::ToolUse {
                    id: "tool_search_1".to_string(),
                    name: "ToolSearch".to_string(),
                    input: json!({"query": "image generation"}),
                }],
            },
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tool_search_1".to_string(),
                    content: json!([{
                        "type": "tool_reference",
                        "tool_name": "mcp__claude-gpt-imagegen__imagegen"
                    }]),
                    is_error: false,
                }],
            },
        ],
        tools: Vec::new(),
        tool_choice: None,
        stream: true,
        stop_sequences: Vec::new(),
    };

    let actual = convert_request(&request, &variant(872_000, 828_400, true), ultra())
        .expect("convert tool-search result");

    assert_eq!(
        actual["input"][1],
        json!({
            "type": "function_call_output",
            "call_id": "tool_search_1",
            "output": "[Tool reference: mcp__claude-gpt-imagegen__imagegen]"
        })
    );
}

#[test]
fn converts_read_image_tool_results_to_responses_input_images() {
    let request = AnthropicRequest {
        model: "claude-gpt-openai::gpt-5.6-sol::872000".to_string(),
        max_tokens: 1024,
        system: Default::default(),
        messages: vec![
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::ToolUse {
                    id: "read_1".to_string(),
                    name: "Read".to_string(),
                    input: json!({"file_path": "/project/image.png"}),
                }],
            },
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "read_1".to_string(),
                    content: json!([{
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/jpeg",
                            "data": "/9j/4AAQSkZJRgABAQ=="
                        }
                    }]),
                    is_error: false,
                }],
            },
        ],
        tools: Vec::new(),
        tool_choice: None,
        stream: true,
        stop_sequences: Vec::new(),
    };

    let actual = convert_request(&request, &variant(872_000, 828_400, true), ultra())
        .expect("convert read image result");

    assert_eq!(
        actual["input"][1],
        json!({
            "type": "function_call_output",
            "call_id": "read_1",
            "output": [{
                "type": "input_image",
                "image_url": "data:image/jpeg;base64,/9j/4AAQSkZJRgABAQ=="
            }]
        })
    );
}

#[test]
fn preserves_plaintext_documents_and_ignores_model_fallback_history() {
    let request: AnthropicRequest = serde_json::from_value(json!({
        "model": "claude-gpt-openai::gpt-5.6-sol::872000",
        "max_tokens": 1024,
        "messages": [
            {
                "role": "assistant",
                "content": [{
                    "type": "fallback",
                    "from": {"model": "claude-sonnet-5"},
                    "to": {"model": "claude-gpt-openai::gpt-5.6-sol::872000"}
                }]
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "document",
                        "title": "notes.txt",
                        "source": {
                            "type": "base64",
                            "media_type": "text/plain",
                            "data": "cHJlc2VydmUgdGhpcyBkb2N1bWVudA=="
                        }
                    },
                    {"type": "text", "text": "Answer from the document."}
                ]
            }
        ]
    }))
    .expect("new Claude Code history is parsed");

    let actual = convert_request(&request, &variant(872_000, 828_400, true), ultra())
        .expect("convert resumed history");

    assert_eq!(
        actual["input"],
        json!([
            {
                "type": "message",
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Attached document `notes.txt` (text/plain):\npreserve this document"
                    },
                    {"type": "input_text", "text": "Answer from the document."}
                ]
            }
        ])
    );
}

#[test]
fn rejects_images_for_a_text_only_model() {
    let request: AnthropicRequest =
        serde_json::from_str(include_str!("fixtures/anthropic_mixed_request.json"))
            .expect("valid Anthropic fixture");

    let error = convert_request(&request, &variant(272_000, 258_400, false), ultra())
        .expect_err("unsupported image must fail");

    assert!(error.to_string().contains("messages[0].content[1]"));
}

#[test]
fn rejects_a_request_over_the_usable_context_without_truncating_history() {
    let request = AnthropicRequest {
        model: "claude-gpt-openai::gpt-5.6-sol::32".to_string(),
        max_tokens: 4,
        system: Default::default(),
        messages: vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: "This prompt is deliberately longer than the tiny usable window.".to_string(),
            }],
        }],
        tools: Vec::new(),
        tool_choice: None,
        stream: true,
        stop_sequences: Vec::new(),
    };
    let before = serde_json::to_value(&request).expect("serialize original request");

    let error = convert_request(&request, &variant(32, 4, false), ultra())
        .expect_err("oversized request must fail");

    assert!(error.to_string().contains("prompt too long"));
    assert_eq!(serde_json::to_value(&request).unwrap(), before);
    assert!(estimate_request_tokens(&request) > 4);
}

#[test]
fn preserves_named_tool_choice() {
    let mut request: AnthropicRequest =
        serde_json::from_str(include_str!("fixtures/anthropic_mixed_request.json"))
            .expect("valid Anthropic fixture");
    request.tool_choice = Some(claude_gpt::anthropic::ToolChoice {
        kind: "tool".to_string(),
        name: Some("weather".to_string()),
        disable_parallel_tool_use: false,
    });

    let actual = convert_request(&request, &variant(872_000, 828_400, true), ultra())
        .expect("convert request");

    assert_eq!(
        actual["tool_choice"],
        json!({"type": "function", "name": "weather"})
    );
}

#[test]
fn converts_claude_system_history_to_responses_developer_messages() {
    let request = AnthropicRequest {
        model: "claude-gpt-openai::gpt-5.6-sol::872000".to_string(),
        max_tokens: 1024,
        system: Default::default(),
        messages: vec![Message {
            role: "system".to_string(),
            content: vec![ContentBlock::Text {
                text: "Keep the model profile active.".to_string(),
            }],
        }],
        tools: Vec::new(),
        tool_choice: None,
        stream: true,
        stop_sequences: Vec::new(),
    };

    let actual = convert_request(&request, &variant(872_000, 828_400, true), ultra())
        .expect("convert system history");

    assert_eq!(actual["input"][0]["role"], "developer");
    assert_eq!(actual["input"][0]["content"][0]["type"], "input_text");
}

#[test]
fn restores_only_versioned_reasoning_from_the_same_model_family() {
    let reasoning = json!({
        "type": "reasoning",
        "id": "rs_1",
        "summary": [],
        "encrypted_content": "opaque"
    });
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&reasoning).unwrap());
    let mut request = AnthropicRequest {
        model: "claude-gpt-openai::gpt-5.6-sol::872000".to_string(),
        max_tokens: 1024,
        system: Default::default(),
        messages: vec![Message {
            role: "assistant".to_string(),
            content: vec![ContentBlock::Thinking {
                thinking: "summary".to_string(),
                signature: format!("openai:v1:gpt-5.6-sol:{encoded}"),
            }],
        }],
        tools: Vec::new(),
        tool_choice: None,
        stream: true,
        stop_sequences: Vec::new(),
    };

    let restored = convert_request(&request, &variant(872_000, 828_400, true), ultra())
        .expect("restore compatible reasoning");
    assert_eq!(restored["input"][0], reasoning);

    request.messages[0].content[0] = ContentBlock::Thinking {
        thinking: "summary".to_string(),
        signature: format!("openai:v1:gpt-5.5:{encoded}"),
    };
    let ignored = convert_request(&request, &variant(872_000, 828_400, true), ultra())
        .expect("ignore incompatible reasoning");
    assert_eq!(ignored["input"], json!([]));
}
