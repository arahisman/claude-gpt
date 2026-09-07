use base64::Engine;
use claude_gpt::sse::{SseTranslator, render_events};
use codex_api::ResponseEvent;
use codex_protocol::ResponseItemId;
use codex_protocol::models::{ContentItem, ResponseItem};
use codex_protocol::protocol::TokenUsage;

fn message(content: Vec<ContentItem>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_call(item_id: &str, call_id: &str, name: &str, arguments: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: Some(ResponseItemId::from_server(item_id.to_string())),
        name: name.to_string(),
        namespace: None,
        arguments: arguments.to_string(),
        encrypted_function_args: None,
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn translate(events: Vec<ResponseEvent>) -> claude_gpt::error::Result<String> {
    let mut translator = SseTranslator::new("msg_test", "gpt-5.6-sol", "gpt-5.6-sol");
    let mut translated = Vec::new();
    for event in events {
        translated.extend(translator.push(event)?);
    }
    translated.extend(translator.finish()?);
    Ok(render_events(&translated))
}

#[test]
fn preserves_text_and_parallel_tool_delta_order() {
    let events = vec![
        ResponseEvent::Created,
        ResponseEvent::OutputItemAdded(message(Vec::new())),
        ResponseEvent::OutputTextDelta("Weather ".to_string()),
        ResponseEvent::OutputTextDelta("ready.".to_string()),
        ResponseEvent::OutputItemDone(message(vec![ContentItem::OutputText {
            text: "Weather ready.".to_string(),
        }])),
        ResponseEvent::OutputItemAdded(function_call("fc_1", "call_weather", "weather", "")),
        ResponseEvent::ToolCallInputDelta {
            item_id: "fc_1".to_string(),
            call_id: Some("call_weather".to_string()),
            delta: "{\"city\":".to_string(),
        },
        ResponseEvent::OutputItemAdded(function_call("fc_2", "call_clock", "clock", "")),
        ResponseEvent::ToolCallInputDelta {
            item_id: "fc_1".to_string(),
            call_id: Some("call_weather".to_string()),
            delta: "\"Paris\"}".to_string(),
        },
        ResponseEvent::ToolCallInputDelta {
            item_id: "fc_2".to_string(),
            call_id: Some("call_clock".to_string()),
            delta: "{\"zone\":\"Europe/Paris\"}".to_string(),
        },
        ResponseEvent::OutputItemDone(function_call(
            "fc_1",
            "call_weather",
            "weather",
            "{\"city\":\"Paris\"}",
        )),
        ResponseEvent::OutputItemDone(function_call(
            "fc_2",
            "call_clock",
            "clock",
            "{\"zone\":\"Europe/Paris\"}",
        )),
        ResponseEvent::Completed {
            response_id: "resp_1".to_string(),
            token_usage: Some(TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 40,
                cache_write_input_tokens: 10,
                output_tokens: 30,
                reasoning_output_tokens: 10,
                total_tokens: 130,
                codex_rollout_budget_units: None,
            }),
            usage_metadata: None,
            end_turn: Some(false),
        },
    ];

    assert_eq!(
        translate(events).expect("translate complete stream"),
        include_str!("fixtures/anthropic_stream.sse")
    );
}

#[test]
fn rejects_eof_before_response_completed() {
    let mut translator = SseTranslator::new("msg_test", "gpt-5.6-sol", "gpt-5.6-sol");
    translator.push(ResponseEvent::Created).unwrap();
    translator
        .push(ResponseEvent::OutputTextDelta("x".to_string()))
        .unwrap();

    let error = translator
        .finish()
        .expect_err("incomplete stream must fail");

    assert!(error.to_string().contains("before response.completed"));
}

#[test]
fn emits_opaque_reasoning_signature_with_the_selected_family() {
    let mut translator = SseTranslator::new("msg_test", "gpt-5.6-sol", "gpt-5.6-sol");
    translator.push(ResponseEvent::Created).unwrap();
    let deltas = translator
        .push(ResponseEvent::ReasoningSummaryDelta {
            delta: "Checking".to_string(),
            summary_index: 0,
        })
        .unwrap();
    assert!(render_events(&deltas).contains("thinking_delta"));

    let item = ResponseItem::Reasoning {
        id: Some(ResponseItemId::from_server("rs_1".to_string())),
        summary: Vec::new(),
        content: None,
        encrypted_content: Some("opaque".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    let done = translator
        .push(ResponseEvent::OutputItemDone(item))
        .expect("finish reasoning item");
    let signature_event = done
        .iter()
        .find(|event| event.data["delta"]["type"] == "signature_delta")
        .expect("signature delta");
    let signature = signature_event.data["delta"]["signature"]
        .as_str()
        .expect("signature string");
    let encoded = signature
        .strip_prefix("openai:v1:gpt-5.6-sol:")
        .expect("versioned family prefix");
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .expect("base64 reasoning");
    let restored: serde_json::Value = serde_json::from_slice(&decoded).expect("reasoning JSON");

    assert_eq!(restored["type"], "reasoning");
    assert_eq!(restored["encrypted_content"], "opaque");
}
