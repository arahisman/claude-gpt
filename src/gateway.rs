use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::future::Future;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use codex_api::ResponseEvent;
use futures::stream::{self, Stream};
use futures::{FutureExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::anthropic::AnthropicRequest;
use crate::catalog::{Catalog, ModelVariant};
use crate::codex_transport::CodexTransport;
use crate::effort::{ClaudeEffort, EffortResolution, resolve_effort};
use crate::error::{BridgeError, Result};
use crate::responses::{convert_request, estimate_request_tokens};
use crate::sse::{AnthropicSseEvent, SseTranslator};
use crate::usage::{UsageSnapshot, render_usage};
const USAGE_COMMAND_MARKER_START: &str = "<claude-gpt-usage>";
const USAGE_COMMAND_MARKER_END: &str = "</claude-gpt-usage>";
const MAX_GATEWAY_REQUEST_BYTES: usize = 32 * 1024 * 1024;

pub type GatewayEventStream =
    Pin<Box<dyn Stream<Item = std::result::Result<ResponseEvent, String>> + Send>>;

pub trait GatewayTransport: Send + Sync + 'static {
    fn stream(
        &self,
        body: Value,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<GatewayEventStream, String>> + Send + '_>>;

    fn rate_limits(
        &self,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<UsageSnapshot, String>> + Send + '_>>;
}

impl GatewayTransport for CodexTransport {
    fn stream(
        &self,
        body: Value,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<GatewayEventStream, String>> + Send + '_>>
    {
        async move {
            let stream = CodexTransport::stream(self, body)
                .await
                .map_err(|error| error.to_string())?;
            Ok(
                Box::pin(stream.map(|event| event.map_err(|error| error.to_string())))
                    as GatewayEventStream,
            )
        }
        .boxed()
    }

    fn rate_limits(
        &self,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<UsageSnapshot, String>> + Send + '_>> {
        async move {
            CodexTransport::rate_limits(self)
                .await
                .map_err(|error| error.to_string())
        }
        .boxed()
    }
}

pub struct SessionState<T: GatewayTransport> {
    transport: Arc<T>,
    catalog: Arc<Catalog>,
    token: String,
}

impl<T: GatewayTransport> SessionState<T> {
    pub fn new(transport: Arc<T>, catalog: Arc<Catalog>, token: String) -> Self {
        Self {
            transport,
            catalog,
            token,
        }
    }

    pub fn with_random_token(transport: Arc<T>, catalog: Arc<Catalog>) -> Result<Self> {
        Ok(Self::new(transport, catalog, random_token()?))
    }
}

struct GatewayState<T: GatewayTransport> {
    transport: Arc<T>,
    catalog: Arc<Catalog>,
    token: Arc<str>,
}

impl<T: GatewayTransport> Clone for GatewayState<T> {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            catalog: self.catalog.clone(),
            token: self.token.clone(),
        }
    }
}

pub struct Gateway;

impl Gateway {
    pub async fn bind<T: GatewayTransport>(state: SessionState<T>) -> Result<RunningGateway> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| {
                BridgeError::CodexTransport(format!("loopback bind failed: {error}"))
            })?;
        let address = listener.local_addr().map_err(|error| {
            BridgeError::CodexTransport(format!("could not read loopback address: {error}"))
        })?;
        let token: Arc<str> = state.token.into();
        let shared = GatewayState {
            transport: state.transport,
            catalog: state.catalog,
            token: token.clone(),
        };
        let router = Router::new()
            .route("/healthz", get(health))
            .route("/v1/models", get(models::<T>))
            .route("/v1/messages", post(messages::<T>))
            .route("/v1/messages/count_tokens", post(count_tokens::<T>))
            .route("/codex/usage", get(usage::<T>))
            .layer(DefaultBodyLimit::max(MAX_GATEWAY_REQUEST_BYTES))
            .with_state(shared);
        let cancellation = CancellationToken::new();
        let shutdown = cancellation.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .map_err(|error| error.to_string())
        });

        Ok(RunningGateway {
            address,
            token,
            cancellation,
            task,
        })
    }
}

pub struct RunningGateway {
    address: SocketAddr,
    token: Arc<str>,
    cancellation: CancellationToken,
    task: JoinHandle<std::result::Result<(), String>>,
}

impl RunningGateway {
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url(), path)
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub async fn ready(&self) -> Result<()> {
        TcpStream::connect(self.address).await.map_err(|error| {
            BridgeError::CodexTransport(format!("gateway readiness check failed: {error}"))
        })?;
        Ok(())
    }

    pub async fn shutdown(self) -> Result<()> {
        self.cancellation.cancel();
        let mut task = self.task;
        match tokio::time::timeout(Duration::from_secs(5), &mut task).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(BridgeError::CodexTransport(error)),
            Ok(Err(error)) => Err(BridgeError::CodexTransport(format!(
                "gateway task failed: {error}"
            ))),
            Err(_) => {
                task.abort();
                let _ = task.await;
                Err(BridgeError::CodexTransport(
                    "gateway did not stop within five seconds".to_string(),
                ))
            }
        }
    }
}

async fn health() -> impl IntoResponse {
    Json(json!({"ok": true}))
}

async fn models<T: GatewayTransport>(
    State(state): State<GatewayState<T>>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&headers, &state.token) {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid token",
        );
    }
    let records = state
        .catalog
        .visible()
        .iter()
        .map(model_record)
        .collect::<Vec<_>>();
    let first_id = records.first().and_then(|record| record["id"].as_str());
    let last_id = records.last().and_then(|record| record["id"].as_str());
    Json(json!({
        "data": records,
        "first_id": first_id,
        "has_more": false,
        "last_id": last_id
    }))
    .into_response()
}

async fn count_tokens<T: GatewayTransport>(
    State(state): State<GatewayState<T>>,
    headers: HeaderMap,
    Json(value): Json<Value>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid token",
        );
    }
    match serde_json::from_value::<AnthropicRequest>(value) {
        Ok(request) => {
            if let Err(error) = state.catalog.resolve(&request.model) {
                return bridge_error(error);
            }
            Json(json!({"input_tokens": estimate_request_tokens(&request)})).into_response()
        }
        Err(error) => api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &format!("invalid request: {error}"),
        ),
    }
}

async fn usage<T: GatewayTransport>(
    State(state): State<GatewayState<T>>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&headers, &state.token) {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid token",
        );
    }
    match state.transport.rate_limits().await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "usage is temporarily unavailable",
        ),
    }
}

async fn messages<T: GatewayTransport>(
    State(state): State<GatewayState<T>>,
    headers: HeaderMap,
    Json(value): Json<Value>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid token",
        );
    }
    let request = match serde_json::from_value::<AnthropicRequest>(value.clone()) {
        Ok(request) => request,
        Err(error) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid request: {error}"),
            );
        }
    };
    let direct_usage = direct_usage_command(&request);
    let usage_output = if direct_usage {
        match state.transport.rate_limits().await {
            Ok(snapshot) => render_usage(&snapshot),
            Err(_) => "Codex subscription usage is temporarily unavailable.".to_string(),
        }
    } else {
        extract_usage_command_output(&request).unwrap_or_default()
    };
    if !usage_output.is_empty() {
        let rate_limits = state.transport.rate_limits().await;
        let message_id = random_token().unwrap_or_else(|_| "unavailable".to_string());
        let events = static_text_events(&request.model, &usage_output, &message_id);
        let response_stream = static_event_stream(events);
        let mut response = Sse::new(response_stream).into_response();
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        apply_usage_headers(response.headers_mut(), rate_limits.as_ref());
        return response;
    }

    let estimated_input_tokens = estimate_request_tokens(&request) as u64;
    let variant = match state
        .catalog
        .resolve_for_input(&request.model, estimated_input_tokens)
    {
        Ok(variant) => variant,
        Err(error) => return bridge_error(error),
    };
    let effort = match request_effort(&value, variant) {
        Ok(effort) => effort,
        Err(error) => return bridge_error(error),
    };
    let response_request = match convert_request(&request, variant, effort) {
        Ok(request) => request,
        Err(error) => return bridge_error(error),
    };
    let rate_limits = state.transport.rate_limits().await;
    let upstream = match state.transport.stream(response_request).await {
        Ok(stream) => stream,
        Err(_) => {
            return api_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "upstream request failed",
            );
        }
    };
    let correlation_id = random_token().unwrap_or_else(|_| "unavailable".to_string());
    let translator = SseTranslator::new(
        format!("msg_{correlation_id}"),
        request.model,
        variant.model_slug.clone(),
    );
    if !request.stream {
        let message = match collect_non_streaming_message(upstream, translator).await {
            Ok(message) => message,
            Err(_) => {
                return api_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "response translation failed",
                );
            }
        };
        let mut response = Json(message).into_response();
        apply_usage_headers(response.headers_mut(), rate_limits.as_ref());
        return response;
    }
    let response_stream = translated_stream(upstream, translator, correlation_id);
    let mut response = Sse::new(response_stream).into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    apply_usage_headers(response.headers_mut(), rate_limits.as_ref());
    response
}

struct NonStreamingMessage {
    message: Option<Value>,
    content: BTreeMap<usize, Value>,
    tool_inputs: BTreeMap<usize, String>,
    stopped: bool,
}

impl NonStreamingMessage {
    fn new() -> Self {
        Self {
            message: None,
            content: BTreeMap::new(),
            tool_inputs: BTreeMap::new(),
            stopped: false,
        }
    }

    fn push(&mut self, event: AnthropicSseEvent) -> Result<()> {
        match event.event.as_str() {
            "message_start" => {
                if self.message.is_some() {
                    return Err(BridgeError::InvalidStream(
                        "received message_start more than once".to_string(),
                    ));
                }
                self.message = Some(event.data.get("message").cloned().ok_or_else(|| {
                    BridgeError::InvalidStream("message_start is missing its message".to_string())
                })?);
            }
            "content_block_start" => {
                let index = event_index(&event.data)?;
                let block = event.data.get("content_block").cloned().ok_or_else(|| {
                    BridgeError::InvalidStream(
                        "content_block_start is missing its content block".to_string(),
                    )
                })?;
                if self.content.insert(index, block).is_some() {
                    return Err(BridgeError::InvalidStream(format!(
                        "received content block index {index} more than once"
                    )));
                }
            }
            "content_block_delta" => self.apply_delta(event.data)?,
            "message_delta" => {
                let message = self.message.as_mut().ok_or_else(|| {
                    BridgeError::InvalidStream(
                        "message_delta arrived before message_start".to_string(),
                    )
                })?;
                let delta = event.data.get("delta").ok_or_else(|| {
                    BridgeError::InvalidStream("message_delta is missing delta".to_string())
                })?;
                message["stop_reason"] = delta.get("stop_reason").cloned().unwrap_or(Value::Null);
                message["stop_sequence"] =
                    delta.get("stop_sequence").cloned().unwrap_or(Value::Null);
                message["usage"] = event.data.get("usage").cloned().unwrap_or(Value::Null);
            }
            "message_stop" => self.stopped = true,
            "content_block_stop" => {}
            unexpected => {
                return Err(BridgeError::InvalidStream(format!(
                    "unexpected Anthropic event `{unexpected}`"
                )));
            }
        }
        Ok(())
    }

    fn apply_delta(&mut self, data: Value) -> Result<()> {
        let index = event_index(&data)?;
        let delta = data.get("delta").ok_or_else(|| {
            BridgeError::InvalidStream("content_block_delta is missing delta".to_string())
        })?;
        let delta_type = delta.get("type").and_then(Value::as_str).ok_or_else(|| {
            BridgeError::InvalidStream("content_block_delta is missing its type".to_string())
        })?;
        let block = self.content.get_mut(&index).ok_or_else(|| {
            BridgeError::InvalidStream(format!(
                "content block delta arrived before block index {index}"
            ))
        })?;
        match delta_type {
            "text_delta" => append_block_text(block, "text", delta, "text")?,
            "thinking_delta" => append_block_text(block, "thinking", delta, "thinking")?,
            "signature_delta" => {
                block["signature"] = delta.get("signature").cloned().ok_or_else(|| {
                    BridgeError::InvalidStream("signature_delta is missing signature".to_string())
                })?;
            }
            "input_json_delta" => {
                let partial = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        BridgeError::InvalidStream(
                            "input_json_delta is missing partial_json".to_string(),
                        )
                    })?;
                self.tool_inputs.entry(index).or_default().push_str(partial);
            }
            unexpected => {
                return Err(BridgeError::InvalidStream(format!(
                    "unsupported content block delta `{unexpected}`"
                )));
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Value> {
        if !self.stopped {
            return Err(BridgeError::InvalidStream(
                "stream ended before message_stop".to_string(),
            ));
        }
        for (index, input) in self.tool_inputs {
            let block = self.content.get_mut(&index).ok_or_else(|| {
                BridgeError::InvalidStream(format!(
                    "tool input refers to missing block index {index}"
                ))
            })?;
            block["input"] = serde_json::from_str(&input).map_err(|error| {
                BridgeError::InvalidStream(format!(
                    "tool input for block index {index} is not valid JSON: {error}"
                ))
            })?;
        }
        let mut message = self.message.ok_or_else(|| {
            BridgeError::InvalidStream("stream ended before message_start".to_string())
        })?;
        message["content"] = Value::Array(self.content.into_values().collect());
        Ok(message)
    }
}

async fn collect_non_streaming_message(
    mut upstream: GatewayEventStream,
    mut translator: SseTranslator,
) -> Result<Value> {
    let mut message = NonStreamingMessage::new();
    while let Some(event) = upstream.next().await {
        let event =
            event.map_err(|_| BridgeError::InvalidStream("upstream stream failed".to_string()))?;
        for translated in translator.push(event)? {
            message.push(translated)?;
        }
    }
    for translated in translator.finish()? {
        message.push(translated)?;
    }
    message.finish()
}

fn event_index(data: &Value) -> Result<usize> {
    data.get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| {
            BridgeError::InvalidStream("event is missing a valid block index".to_string())
        })
}

fn append_block_text(
    block: &mut Value,
    field: &str,
    delta: &Value,
    delta_field: &str,
) -> Result<()> {
    let addition = delta
        .get(delta_field)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            BridgeError::InvalidStream(format!("{delta_field} is missing from content block delta"))
        })?;
    let previous = block.get(field).and_then(Value::as_str).unwrap_or_default();
    block[field] = Value::String(format!("{previous}{addition}"));
    Ok(())
}

fn translated_stream(
    upstream: GatewayEventStream,
    translator: SseTranslator,
    correlation_id: String,
) -> impl Stream<Item = std::result::Result<Event, Infallible>> {
    struct TranslationState {
        upstream: GatewayEventStream,
        translator: SseTranslator,
        pending: VecDeque<AnthropicSseEvent>,
        correlation_id: String,
        finished: bool,
    }

    stream::unfold(
        TranslationState {
            upstream,
            translator,
            pending: VecDeque::new(),
            correlation_id,
            finished: false,
        },
        |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    let event = Event::default()
                        .event(event.event)
                        .json_data(event.data)
                        .expect("JSON Value serializes");
                    return Some((Ok(event), state));
                }
                if state.finished {
                    return None;
                }
                match state.upstream.next().await {
                    Some(Ok(event)) => match state.translator.push(event) {
                        Ok(events) => state.pending.extend(events),
                        Err(error) => {
                            state
                                .pending
                                .push_back(stream_error(&state.correlation_id, &error.to_string()));
                            state.finished = true;
                        }
                    },
                    Some(Err(_)) => {
                        state.pending.push_back(stream_error(
                            &state.correlation_id,
                            "upstream stream failed",
                        ));
                        state.finished = true;
                    }
                    None => {
                        match state.translator.finish() {
                            Ok(events) => state.pending.extend(events),
                            Err(error) => state
                                .pending
                                .push_back(stream_error(&state.correlation_id, &error.to_string())),
                        }
                        state.finished = true;
                    }
                }
            }
        },
    )
}

fn static_event_stream(
    events: Vec<AnthropicSseEvent>,
) -> impl Stream<Item = std::result::Result<Event, Infallible>> {
    stream::iter(events.into_iter().map(|event| {
        Ok(Event::default()
            .event(event.event)
            .json_data(event.data)
            .expect("JSON Value serializes"))
    }))
}

fn static_text_events(model: &str, text: &str, message_id: &str) -> Vec<AnthropicSseEvent> {
    vec![
        sse_event(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": crate::sse::AnthropicUsage::default()
                }
            }),
        ),
        sse_event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
        ),
        sse_event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text}
            }),
        ),
        sse_event(
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": 0
            }),
        ),
        sse_event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": crate::sse::AnthropicUsage::default()
            }),
        ),
        sse_event("message_stop", json!({"type": "message_stop"})),
    ]
}

fn sse_event(event: &str, data: Value) -> AnthropicSseEvent {
    AnthropicSseEvent {
        event: event.to_string(),
        data,
    }
}

fn extract_usage_command_output(request: &AnthropicRequest) -> Option<String> {
    let message = request.messages.last()?;
    if message.role != "user" {
        return None;
    }
    let content = match message.content.as_slice() {
        [crate::anthropic::ContentBlock::Text { text }] => text,
        _ => return None,
    };
    let start = content.find(USAGE_COMMAND_MARKER_START)?;
    let after_start = start + USAGE_COMMAND_MARKER_START.len();
    let end = content[after_start..].find(USAGE_COMMAND_MARKER_END)? + after_start;
    if content[end + USAGE_COMMAND_MARKER_END.len()..].contains(USAGE_COMMAND_MARKER_START) {
        return None;
    }
    let output = content[after_start..end].trim();
    (!output.is_empty()).then(|| output.to_string())
}

fn direct_usage_command(request: &AnthropicRequest) -> bool {
    let Some(message) = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
    else {
        return false;
    };
    message.content.iter().any(|content| {
        matches!(
            content,
            crate::anthropic::ContentBlock::Text { text }
                if text.trim() == "/codex-usage"
                    || text.contains("<command-name>/codex-usage</command-name>")
        )
    })
}

fn stream_error(correlation_id: &str, message: &str) -> AnthropicSseEvent {
    AnthropicSseEvent {
        event: "error".to_string(),
        data: json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": format!("{} [correlation id: {}]", sanitize_error(message), correlation_id)
            }
        }),
    }
}

fn request_effort(value: &Value, variant: &ModelVariant) -> Result<EffortResolution> {
    let requested = value
        .pointer("/output_config/effort")
        .cloned()
        .map(serde_json::from_value::<ClaudeEffort>)
        .transpose()
        .map_err(|error| BridgeError::InvalidRequest {
            path: "output_config.effort".to_string(),
            message: error.to_string(),
        })?;
    Ok(match requested {
        Some(requested) => resolve_effort(
            requested,
            &variant.supported_efforts,
            variant.default_effort.clone(),
        ),
        None => EffortResolution {
            requested: ClaudeEffort::Medium,
            actual: variant.default_effort.clone(),
            fell_back: false,
        },
    })
}

fn model_record(variant: &ModelVariant) -> Value {
    let supported = &variant.supported_efforts;
    let mut effort = Map::new();
    effort.insert("supported".to_string(), Value::Bool(!supported.is_empty()));
    for level in ["low", "medium", "high", "xhigh"] {
        effort.insert(
            level.to_string(),
            json!({"supported": supported.iter().any(|value| value.as_str() == level)}),
        );
    }
    effort.insert(
        "max".to_string(),
        json!({
            "supported": supported.iter().any(|value| matches!(value.as_str(), "max" | "ultra"))
        }),
    );
    json!({
        "id": variant.gateway_id,
        "created_at": "1970-01-01T00:00:00Z",
        "display_name": variant.display_name,
        "max_input_tokens": variant.usable_tokens,
        "max_tokens": null,
        "type": "model",
        "capabilities": {
            "effort": effort,
            "image_input": {"supported": variant.supports_images},
            "thinking": {"supported": !supported.is_empty()}
        }
    })
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    let Some(actual) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };
    constant_time_equal(actual.as_bytes(), expected.as_bytes())
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn apply_usage_headers(
    headers: &mut HeaderMap,
    usage: std::result::Result<&UsageSnapshot, &String>,
) {
    match usage {
        Ok(snapshot) => {
            if let Some(limit) = snapshot
                .rate_limits
                .iter()
                .find(|limit| limit.limit_id.as_deref() == Some("codex"))
                .or_else(|| snapshot.rate_limits.first())
            {
                insert_header(headers, "x-claude-gpt-limit-id", limit.limit_id.as_deref());
                if let Some(primary) = &limit.primary {
                    insert_header(
                        headers,
                        "x-claude-gpt-primary-used-percent",
                        Some(primary.used_percent.to_string().as_str()),
                    );
                    insert_header(
                        headers,
                        "x-claude-gpt-primary-reset",
                        primary.resets_at.map(|value| value.to_string()).as_deref(),
                    );
                }
            }
        }
        Err(_) => {
            headers.insert(
                HeaderName::from_static("x-claude-gpt-usage-status"),
                HeaderValue::from_static("unavailable"),
            );
        }
    }
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: Option<&str>) {
    if let Some(value) = value.and_then(|value| HeaderValue::from_str(value).ok()) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

fn bridge_error(error: BridgeError) -> Response {
    let status = match error {
        BridgeError::UnknownModel(_)
        | BridgeError::InvalidRequest { .. }
        | BridgeError::PromptTooLong { .. } => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    api_error(status, "invalid_request_error", &error.to_string())
}

fn api_error(status: StatusCode, kind: &str, message: &str) -> Response {
    let correlation_id = random_token().unwrap_or_else(|_| "unavailable".to_string());
    (
        status,
        Json(json!({
            "type": "error",
            "error": {
                "type": kind,
                "message": format!(
                    "{} [correlation id: {}]",
                    sanitize_error(message),
                    correlation_id
                )
            }
        })),
    )
        .into_response()
}

fn sanitize_error(message: &str) -> String {
    message
        .chars()
        .filter(|character| !character.is_control())
        .take(500)
        .collect()
}

fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| BridgeError::CodexTransport(format!("random token failed: {error}")))?;
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        bytes,
    ))
}
