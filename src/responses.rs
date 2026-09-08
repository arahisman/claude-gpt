use std::collections::HashSet;

use base64::Engine;
use codex_utils_output_truncation::approx_token_count;
use serde_json::{Value, json};

use crate::anthropic::{
    AnthropicRequest, ContentBlock, DocumentSource, ImageSource, Message, ToolChoice,
    ToolDefinition,
};
use crate::catalog::ModelVariant;
use crate::effort::{EffortResolution, wire_effort};
use crate::error::{BridgeError, Result};

const IMAGE_TOKEN_ESTIMATE: usize = 4_096;

pub fn convert_request(
    request: &AnthropicRequest,
    variant: &ModelVariant,
    effort: EffortResolution,
) -> Result<Value> {
    let estimated_tokens = estimate_request_tokens(request) as u64;
    let limit_tokens = u64::try_from(variant.usable_tokens).unwrap_or_default();
    if estimated_tokens > limit_tokens {
        return Err(BridgeError::PromptTooLong {
            estimated_tokens,
            limit_tokens,
        });
    }

    let mut input = Vec::new();
    let mut known_tool_calls = HashSet::new();
    for (message_index, message) in request.messages.iter().enumerate() {
        convert_message(
            message,
            message_index,
            variant,
            &mut known_tool_calls,
            &mut input,
        )?;
    }

    let tools = request
        .tools
        .iter()
        .map(convert_tool)
        .collect::<Result<Vec<_>>>()?;
    let (tool_choice, parallel_tool_calls) =
        convert_tool_choice(request.tool_choice.as_ref(), &request.tools)?;
    let wire_effort = wire_effort(
        &effort.actual,
        &variant.supported_efforts,
        variant.multi_agent_reasoning_effort.as_ref(),
    );

    Ok(json!({
        "model": variant.model_slug,
        "instructions": request.system.text(),
        "input": input,
        "tools": tools,
        "tool_choice": tool_choice,
        "parallel_tool_calls": parallel_tool_calls,
        "reasoning": {
            "effort": wire_effort.as_str(),
            "summary": "auto"
        },
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"]
    }))
}

pub fn estimate_request_tokens(request: &AnthropicRequest) -> usize {
    let mut tokens = approx_token_count(&request.system.text());
    tokens = tokens.saturating_add(request.messages.len().saturating_mul(8));
    for message in &request.messages {
        for block in &message.content {
            tokens = tokens.saturating_add(match block {
                ContentBlock::Text { text } => approx_token_count(text),
                ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
                ContentBlock::Document { source, .. } => estimate_document_tokens(source),
                ContentBlock::ToolUse { name, input, .. } => approx_token_count(name)
                    .saturating_add(approx_token_count(&input.to_string()))
                    .saturating_add(16),
                ContentBlock::ToolResult { content, .. } => {
                    estimate_tool_result_tokens(content).saturating_add(16)
                }
                ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::Fallback => 0,
            });
        }
    }
    for tool in &request.tools {
        tokens = tokens
            .saturating_add(approx_token_count(&tool.name))
            .saturating_add(approx_token_count(&tool.description))
            .saturating_add(approx_token_count(&tool.input_schema.to_string()))
            .saturating_add(32);
    }
    tokens
}

fn convert_message(
    message: &Message,
    message_index: usize,
    variant: &ModelVariant,
    known_tool_calls: &mut HashSet<String>,
    input: &mut Vec<Value>,
) -> Result<()> {
    let responses_role = match message.role.as_str() {
        "user" => "user",
        "assistant" => "assistant",
        "system" => "developer",
        _ => {
            return invalid_request(
                format!("messages[{message_index}].role"),
                format!("unsupported role `{}`", message.role),
            );
        }
    };

    let mut message_content = Vec::new();
    for (content_index, block) in message.content.iter().enumerate() {
        let path = format!("messages[{message_index}].content[{content_index}]");
        match block {
            ContentBlock::Text { text } => message_content.push(json!({
                "type": if message.role == "assistant" { "output_text" } else { "input_text" },
                "text": text
            })),
            ContentBlock::Image { source } => {
                if message.role != "user" {
                    return invalid_request(path, "images are only valid in user messages");
                }
                if !variant.supports_images {
                    return invalid_request(path, "selected model does not support image input");
                }
                message_content.push(json!({
                    "type": "input_image",
                    "image_url": image_url(source)
                }));
            }
            ContentBlock::Document { title, source } => {
                if message.role != "user" {
                    return invalid_request(path, "documents are only valid in user messages");
                }
                message_content.push(json!({
                    "type": "input_text",
                    "text": document_text(title, source, &path)?
                }));
            }
            ContentBlock::ToolUse {
                id,
                name,
                input: arguments,
            } => {
                flush_message(responses_role, &mut message_content, input);
                if message.role != "assistant" {
                    return invalid_request(path, "tool_use is only valid in assistant messages");
                }
                if !known_tool_calls.insert(id.clone()) {
                    return invalid_request(format!("{path}.id"), "duplicate tool call ID");
                }
                input.push(json!({
                    "type": "function_call",
                    "name": name,
                    "arguments": serde_json::to_string(arguments).map_err(|source| BridgeError::SerializeJson {
                        path: std::path::PathBuf::from(&path),
                        source,
                    })?,
                    "call_id": id
                }));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                flush_message(responses_role, &mut message_content, input);
                if message.role != "user" {
                    return invalid_request(path, "tool_result is only valid in user messages");
                }
                if !known_tool_calls.contains(tool_use_id) {
                    return invalid_request(
                        format!("{path}.tool_use_id"),
                        format!("no preceding tool_use has ID `{tool_use_id}`"),
                    );
                }
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": convert_tool_result(content, &path)?
                }));
            }
            ContentBlock::Thinking { signature, .. } => {
                flush_message(responses_role, &mut message_content, input);
                if let Some(reasoning) = decode_reasoning(signature, &variant.model_slug) {
                    input.push(reasoning);
                }
            }
            ContentBlock::RedactedThinking { .. } => {
                flush_message(responses_role, &mut message_content, input);
            }
            ContentBlock::Fallback => {
                flush_message(responses_role, &mut message_content, input);
            }
        }
    }
    flush_message(responses_role, &mut message_content, input);
    Ok(())
}

fn flush_message(role: &str, content: &mut Vec<Value>, input: &mut Vec<Value>) {
    if content.is_empty() {
        return;
    }
    input.push(json!({
        "type": "message",
        "role": role,
        "content": std::mem::take(content)
    }));
}

fn convert_tool(tool: &ToolDefinition) -> Result<Value> {
    let schema = tool
        .input_schema
        .as_object()
        .ok_or_else(|| BridgeError::InvalidRequest {
            path: format!("tools[{}].input_schema", tool.name),
            message: "tool input_schema must be a JSON object".to_string(),
        })?;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return invalid_request(
            format!("tools[{}].input_schema.type", tool.name),
            "tool input_schema type must be `object`",
        );
    }
    Ok(json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "strict": false,
        "parameters": tool.input_schema
    }))
}

fn convert_tool_choice(
    choice: Option<&ToolChoice>,
    tools: &[ToolDefinition],
) -> Result<(Value, bool)> {
    let Some(choice) = choice else {
        return Ok((Value::String("auto".to_string()), true));
    };
    let value = match choice.kind.as_str() {
        "auto" => Value::String("auto".to_string()),
        "any" => Value::String("required".to_string()),
        "none" => Value::String("none".to_string()),
        "tool" => {
            let name = choice
                .name
                .as_deref()
                .ok_or_else(|| BridgeError::InvalidRequest {
                    path: "tool_choice.name".to_string(),
                    message: "named tool choice requires a name".to_string(),
                })?;
            if !tools.iter().any(|tool| tool.name == name) {
                return invalid_request("tool_choice.name", format!("unknown tool `{name}`"));
            }
            json!({"type": "function", "name": name})
        }
        other => {
            return invalid_request(
                "tool_choice.type",
                format!("unsupported tool choice `{other}`"),
            );
        }
    };
    Ok((value, !choice.disable_parallel_tool_use))
}

fn image_url(source: &ImageSource) -> String {
    match source {
        ImageSource::Base64 { media_type, data } => {
            format!("data:{media_type};base64,{data}")
        }
        ImageSource::Url { url } => url.clone(),
    }
}

fn estimate_document_tokens(source: &DocumentSource) -> usize {
    match source {
        DocumentSource::Base64 { data, .. } => data.len().saturating_mul(3) / 16,
        DocumentSource::Text { data, .. } => approx_token_count(data),
        DocumentSource::Url { url } => approx_token_count(url),
    }
}

fn document_text(title: &str, source: &DocumentSource, path: &str) -> Result<String> {
    let (media_type, text) = match source {
        DocumentSource::Base64 { media_type, data } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|error| BridgeError::InvalidRequest {
                    path: format!("{path}.source.data"),
                    message: format!("document base64 is invalid: {error}"),
                })?;
            let text = String::from_utf8(bytes).map_err(|error| BridgeError::InvalidRequest {
                path: format!("{path}.source.data"),
                message: format!("document text is not UTF-8: {error}"),
            })?;
            (media_type, text)
        }
        DocumentSource::Text { media_type, data } => (media_type, data.clone()),
        DocumentSource::Url { .. } => {
            return invalid_request(path, "document source must use inline text or base64 data");
        }
    };
    if media_type != "text/plain" {
        return invalid_request(
            format!("{path}.source.media_type"),
            format!("unsupported document media type `{media_type}`"),
        );
    }
    Ok(format!(
        "Attached document `{title}` ({media_type}):\n{text}"
    ))
}

fn convert_tool_result(content: &Value, path: &str) -> Result<Value> {
    match content {
        Value::Null => Ok(Value::String(String::new())),
        Value::String(text) => Ok(Value::String(text.clone())),
        Value::Array(blocks) => {
            let mut output = Vec::new();
            for (index, block) in blocks.iter().enumerate() {
                let kind = block.get("type").and_then(Value::as_str).ok_or_else(|| {
                    BridgeError::InvalidRequest {
                        path: format!("{path}.content[{index}].type"),
                        message: "content block type is required".to_string(),
                    }
                })?;
                match kind {
                    "text" => output.push(json!({
                        "type": "input_text",
                        "text": block.get("text").and_then(Value::as_str).unwrap_or_default()
                    })),
                    "tool_reference" => {
                        let tool_name =
                            block
                                .get("tool_name")
                                .and_then(Value::as_str)
                                .ok_or_else(|| BridgeError::InvalidRequest {
                                    path: format!("{path}.content[{index}].tool_name"),
                                    message: "tool_reference requires a tool_name".to_string(),
                                })?;
                        output.push(json!({
                            "type": "input_text",
                            "text": format!("[Tool reference: {tool_name}]")
                        }));
                    }
                    "image" => {
                        output.push(json!({
                            "type": "input_image",
                            "image_url": tool_result_image_url(block, path, index)?
                        }));
                    }
                    _ => {
                        return invalid_request(
                            format!("{path}.content[{index}].type"),
                            format!("unsupported tool result content `{kind}`"),
                        );
                    }
                }
            }
            if output.iter().all(|item| item["type"] == "input_text") {
                Ok(Value::String(
                    output
                        .iter()
                        .filter_map(|item| item["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ))
            } else {
                Ok(Value::Array(output))
            }
        }
        other => serde_json::to_string(other)
            .map(Value::String)
            .map_err(|source| BridgeError::SerializeJson {
                path: std::path::PathBuf::from(path),
                source,
            }),
    }
}

fn tool_result_image_url(block: &Value, path: &str, index: usize) -> Result<String> {
    let source = block
        .get("source")
        .ok_or_else(|| BridgeError::InvalidRequest {
            path: format!("{path}.content[{index}].source"),
            message: "image content requires a source".to_string(),
        })?;
    let source = serde_json::from_value::<ImageSource>(source.clone()).map_err(|error| {
        BridgeError::InvalidRequest {
            path: format!("{path}.content[{index}].source"),
            message: format!("invalid image source: {error}"),
        }
    })?;
    match source {
        ImageSource::Base64 { .. } => Ok(image_url(&source)),
        ImageSource::Url { .. } => invalid_request(
            format!("{path}.content[{index}].source"),
            "tool result images must use base64 data",
        ),
    }
}

fn estimate_tool_result_tokens(content: &Value) -> usize {
    match content {
        Value::String(text) => approx_token_count(text),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map_or(IMAGE_TOKEN_ESTIMATE, approx_token_count)
            })
            .sum(),
        other => approx_token_count(&other.to_string()),
    }
}

fn decode_reasoning(signature: &str, model_family: &str) -> Option<Value> {
    let encoded = signature.strip_prefix(&format!("openai:v1:{model_family}:"))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    (value.get("type").and_then(Value::as_str) == Some("reasoning")).then_some(value)
}

fn invalid_request<T>(path: impl Into<String>, message: impl Into<String>) -> Result<T> {
    Err(BridgeError::InvalidRequest {
        path: path.into(),
        message: message.into(),
    })
}
