use std::collections::HashMap;

use base64::Engine;
use codex_api::ResponseEvent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{BridgeError, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct AnthropicSseEvent {
    pub event: String,
    pub data: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicUsage {
    pub input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub output_tokens: i64,
}

pub struct SseTranslator {
    message_id: String,
    model: String,
    model_family: String,
    started: bool,
    completed: bool,
    next_index: usize,
    text_index: Option<usize>,
    reasoning_index: Option<usize>,
    tools: HashMap<String, ToolBlock>,
    tool_order: Vec<String>,
}

struct ToolBlock {
    index: usize,
    call_id: String,
    name: String,
    arguments: String,
    closed: bool,
}

impl SseTranslator {
    pub fn new(
        message_id: impl Into<String>,
        model: impl Into<String>,
        model_family: impl Into<String>,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            model: model.into(),
            model_family: model_family.into(),
            started: false,
            completed: false,
            next_index: 0,
            text_index: None,
            reasoning_index: None,
            tools: HashMap::new(),
            tool_order: Vec::new(),
        }
    }

    pub fn push(&mut self, event: ResponseEvent) -> Result<Vec<AnthropicSseEvent>> {
        if self.completed {
            return Err(BridgeError::InvalidStream(
                "received an event after response.completed".to_string(),
            ));
        }

        let mut output = Vec::new();
        match event {
            ResponseEvent::Created => {
                if self.started {
                    return Err(BridgeError::InvalidStream(
                        "received response.created more than once".to_string(),
                    ));
                }
                self.started = true;
                output.push(sse_event(
                    "message_start",
                    json!({
                        "type": "message_start",
                        "message": {
                            "id": self.message_id,
                            "type": "message",
                            "role": "assistant",
                            "model": self.model,
                            "content": [],
                            "stop_reason": null,
                            "stop_sequence": null,
                            "usage": AnthropicUsage::default()
                        }
                    }),
                ));
            }
            ResponseEvent::OutputItemAdded(item) => {
                self.require_started()?;
                match item {
                    ResponseItem::Message { .. } => self.start_text(&mut output),
                    ResponseItem::FunctionCall {
                        id,
                        call_id,
                        name,
                        arguments,
                        ..
                    } => {
                        let item_id = id.map(String::from).ok_or_else(|| {
                            BridgeError::InvalidStream(
                                "function_call item is missing its item ID".to_string(),
                            )
                        })?;
                        self.start_tool(item_id, call_id, name, arguments, &mut output)?;
                    }
                    _ => {}
                }
            }
            ResponseEvent::OutputTextDelta(delta) => {
                self.require_started()?;
                self.start_text(&mut output);
                let index = self.text_index.expect("text block was just started");
                output.push(sse_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": delta}
                    }),
                ));
            }
            ResponseEvent::ToolCallInputDelta {
                item_id,
                call_id,
                delta,
            } => {
                self.require_started()?;
                let tool = self.tools.get_mut(&item_id).ok_or_else(|| {
                    BridgeError::InvalidStream(format!(
                        "tool arguments arrived before function_call `{item_id}`"
                    ))
                })?;
                if let Some(call_id) = call_id
                    && call_id != tool.call_id
                {
                    return Err(BridgeError::InvalidStream(format!(
                        "tool call ID changed for item `{item_id}`"
                    )));
                }
                tool.arguments.push_str(&delta);
                output.push(sse_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": tool.index,
                        "delta": {"type": "input_json_delta", "partial_json": delta}
                    }),
                ));
            }
            ResponseEvent::ReasoningSummaryDelta { delta, .. } => {
                self.require_started()?;
                self.start_reasoning(&mut output);
                let index = self
                    .reasoning_index
                    .expect("reasoning block was just started");
                output.push(sse_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "thinking_delta", "thinking": delta}
                    }),
                ));
            }
            ResponseEvent::OutputItemDone(item) => match item {
                ResponseItem::Message { .. } => self.stop_text(&mut output),
                ResponseItem::FunctionCall {
                    id,
                    call_id,
                    name,
                    arguments,
                    ..
                } => {
                    let item_id = id.map(String::from).ok_or_else(|| {
                        BridgeError::InvalidStream(
                            "completed function_call is missing its item ID".to_string(),
                        )
                    })?;
                    if !self.tools.contains_key(&item_id) {
                        self.start_tool(
                            item_id.clone(),
                            call_id.clone(),
                            name.clone(),
                            String::new(),
                            &mut output,
                        )?;
                    }
                    let tool = self.tools.get_mut(&item_id).expect("tool was started");
                    if tool.call_id != call_id || tool.name != name {
                        return Err(BridgeError::InvalidStream(format!(
                            "completed function_call metadata changed for item `{item_id}`"
                        )));
                    }
                    if tool.arguments.is_empty() && !arguments.is_empty() {
                        tool.arguments = arguments.clone();
                        output.push(sse_event(
                            "content_block_delta",
                            json!({
                                "type": "content_block_delta",
                                "index": tool.index,
                                "delta": {"type": "input_json_delta", "partial_json": arguments}
                            }),
                        ));
                    } else if !arguments.is_empty() && tool.arguments != arguments {
                        return Err(BridgeError::InvalidStream(format!(
                            "completed function_call arguments differ for item `{item_id}`"
                        )));
                    }
                    if !tool.closed {
                        tool.closed = true;
                        output.push(block_stop(tool.index));
                    }
                }
                ResponseItem::Reasoning { .. } => {
                    self.start_reasoning(&mut output);
                    let index = self.reasoning_index.expect("reasoning block was started");
                    let bytes =
                        serde_json::to_vec(&item).map_err(|source| BridgeError::SerializeJson {
                            path: std::path::PathBuf::from("response.reasoning"),
                            source,
                        })?;
                    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
                    output.push(sse_event(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": {
                                "type": "signature_delta",
                                "signature": format!("openai:v1:{}:{encoded}", self.model_family)
                            }
                        }),
                    ));
                    output.push(block_stop(index));
                    self.reasoning_index = None;
                }
                _ => {}
            },
            ResponseEvent::Completed {
                token_usage,
                end_turn,
                ..
            } => {
                self.require_started()?;
                self.stop_text(&mut output);
                if self.reasoning_index.is_some() {
                    return Err(BridgeError::InvalidStream(
                        "response.completed arrived before the reasoning item completed"
                            .to_string(),
                    ));
                }
                for item_id in self.tool_order.clone() {
                    let tool = self.tools.get_mut(&item_id).expect("ordered tool exists");
                    if !tool.closed {
                        tool.closed = true;
                        output.push(block_stop(tool.index));
                    }
                }
                let stop_reason = if !self.tools.is_empty() {
                    "tool_use"
                } else if end_turn == Some(false) {
                    "pause_turn"
                } else {
                    "end_turn"
                };
                let usage = token_usage.map_or_else(AnthropicUsage::default, map_usage);
                output.push(sse_event(
                    "message_delta",
                    json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                        "usage": usage
                    }),
                ));
                output.push(sse_event("message_stop", json!({"type": "message_stop"})));
                self.completed = true;
            }
            ResponseEvent::ReasoningSummaryDone { .. }
            | ResponseEvent::ReasoningContentDelta { .. }
            | ResponseEvent::ReasoningSummaryPartAdded { .. }
            | ResponseEvent::SafetyBuffering(_)
            | ResponseEvent::ServerModel(_)
            | ResponseEvent::ModelVerifications(_)
            | ResponseEvent::TurnModerationMetadata(_)
            | ResponseEvent::ServerReasoningIncluded(_)
            | ResponseEvent::RateLimits(_)
            | ResponseEvent::ModelsEtag(_) => {}
        }
        Ok(output)
    }

    pub fn finish(&self) -> Result<Vec<AnthropicSseEvent>> {
        if self.completed {
            Ok(Vec::new())
        } else {
            Err(BridgeError::InvalidStream(
                "stream ended before response.completed".to_string(),
            ))
        }
    }

    fn require_started(&self) -> Result<()> {
        if self.started {
            Ok(())
        } else {
            Err(BridgeError::InvalidStream(
                "received output before response.created".to_string(),
            ))
        }
    }

    fn start_text(&mut self, output: &mut Vec<AnthropicSseEvent>) {
        if self.text_index.is_some() {
            return;
        }
        let index = self.take_index();
        self.text_index = Some(index);
        output.push(sse_event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "text", "text": ""}
            }),
        ));
    }

    fn stop_text(&mut self, output: &mut Vec<AnthropicSseEvent>) {
        if let Some(index) = self.text_index.take() {
            output.push(block_stop(index));
        }
    }

    fn start_reasoning(&mut self, output: &mut Vec<AnthropicSseEvent>) {
        if self.reasoning_index.is_some() {
            return;
        }
        let index = self.take_index();
        self.reasoning_index = Some(index);
        output.push(sse_event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "thinking", "thinking": ""}
            }),
        ));
    }

    fn start_tool(
        &mut self,
        item_id: String,
        call_id: String,
        name: String,
        arguments: String,
        output: &mut Vec<AnthropicSseEvent>,
    ) -> Result<()> {
        if self.tools.contains_key(&item_id) {
            return Err(BridgeError::InvalidStream(format!(
                "duplicate function_call item `{item_id}`"
            )));
        }
        let index = self.take_index();
        output.push(sse_event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "tool_use",
                    "id": call_id,
                    "name": name,
                    "input": {}
                }
            }),
        ));
        if !arguments.is_empty() {
            output.push(sse_event(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": arguments}
                }),
            ));
        }
        self.tool_order.push(item_id.clone());
        self.tools.insert(
            item_id,
            ToolBlock {
                index,
                call_id,
                name,
                arguments,
                closed: false,
            },
        );
        Ok(())
    }

    fn take_index(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        index
    }
}

pub fn render_events(events: &[AnthropicSseEvent]) -> String {
    let mut output = String::new();
    for event in events {
        output.push_str("event: ");
        output.push_str(&event.event);
        output.push('\n');
        output.push_str("data: ");
        output.push_str(&serde_json::to_string(&event.data).expect("JSON Value serializes"));
        output.push_str("\n\n");
    }
    output
}

fn map_usage(usage: TokenUsage) -> AnthropicUsage {
    AnthropicUsage {
        input_tokens: usage
            .input_tokens
            .saturating_sub(usage.cached_input_tokens)
            .saturating_sub(usage.cache_write_input_tokens),
        cache_creation_input_tokens: usage.cache_write_input_tokens,
        cache_read_input_tokens: usage.cached_input_tokens,
        output_tokens: usage
            .output_tokens
            .saturating_sub(usage.reasoning_output_tokens),
    }
}

fn sse_event(event: &str, data: Value) -> AnthropicSseEvent {
    AnthropicSseEvent {
        event: event.to_string(),
        data,
    }
}

fn block_stop(index: usize) -> AnthropicSseEvent {
    sse_event(
        "content_block_stop",
        json!({"type": "content_block_stop", "index": index}),
    )
}
