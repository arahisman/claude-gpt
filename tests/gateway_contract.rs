use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use claude_gpt::catalog::Catalog;
use claude_gpt::gateway::{Gateway, GatewayEventStream, GatewayTransport, SessionState};
use claude_gpt::usage::UsageSnapshot;
use codex_api::ResponseEvent;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelsResponse;
use futures::{Stream, StreamExt, stream};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

#[derive(Default)]
struct MockTransport {
    requests: Mutex<Vec<Value>>,
}

struct ErrorTransport;

struct DisconnectTransport {
    dropped: Arc<AtomicBool>,
}

struct DropAwareStream {
    emitted: bool,
    dropped: Arc<AtomicBool>,
}

fn assistant_message() -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: Vec::new(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

impl Stream for DropAwareStream {
    type Item = Result<ResponseEvent, String>;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.emitted {
            Poll::Pending
        } else {
            self.emitted = true;
            Poll::Ready(Some(Ok(ResponseEvent::Created)))
        }
    }
}

impl Drop for DropAwareStream {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

impl GatewayTransport for ErrorTransport {
    fn stream(
        &self,
        _body: Value,
    ) -> Pin<Box<dyn Future<Output = Result<GatewayEventStream, String>> + Send + '_>> {
        Box::pin(async { Err("Bearer oauth-secret from tool payload".to_string()) })
    }

    fn rate_limits(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<UsageSnapshot, String>> + Send + '_>> {
        Box::pin(async { Ok(UsageSnapshot::new(Vec::new())) })
    }
}

impl GatewayTransport for DisconnectTransport {
    fn stream(
        &self,
        _body: Value,
    ) -> Pin<Box<dyn Future<Output = Result<GatewayEventStream, String>> + Send + '_>> {
        let stream = DropAwareStream {
            emitted: false,
            dropped: self.dropped.clone(),
        };
        Box::pin(async move { Ok(Box::pin(stream) as GatewayEventStream) })
    }

    fn rate_limits(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<UsageSnapshot, String>> + Send + '_>> {
        Box::pin(async { Ok(UsageSnapshot::new(Vec::new())) })
    }
}

impl GatewayTransport for MockTransport {
    fn stream(
        &self,
        body: Value,
    ) -> Pin<Box<dyn Future<Output = Result<GatewayEventStream, String>> + Send + '_>> {
        self.requests.lock().unwrap().push(body);
        Box::pin(async move {
            let events = vec![
                Ok(ResponseEvent::Created),
                Ok(ResponseEvent::OutputItemAdded(assistant_message())),
                Ok(ResponseEvent::OutputTextDelta(
                    "Checking the weather.".to_string(),
                )),
                Ok(ResponseEvent::OutputItemDone(assistant_message())),
                Ok(ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall {
                    id: Some(ResponseItemId::from_server("fc_1".to_string())),
                    name: "weather".to_string(),
                    namespace: None,
                    arguments: "{\"city\":\"Paris\"}".to_string(),
                    encrypted_function_args: None,
                    call_id: "call_1".to_string(),
                    internal_chat_message_metadata_passthrough: None,
                })),
                Ok(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                    id: Some(ResponseItemId::from_server("fc_1".to_string())),
                    name: "weather".to_string(),
                    namespace: None,
                    arguments: "{\"city\":\"Paris\"}".to_string(),
                    encrypted_function_args: None,
                    call_id: "call_1".to_string(),
                    internal_chat_message_metadata_passthrough: None,
                })),
                Ok(ResponseEvent::Completed {
                    response_id: "resp_1".to_string(),
                    token_usage: None,
                    usage_metadata: None,
                    end_turn: Some(true),
                }),
            ];
            Ok(Box::pin(stream::iter(events)) as GatewayEventStream)
        })
    }

    fn rate_limits(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<UsageSnapshot, String>> + Send + '_>> {
        Box::pin(async { Ok(UsageSnapshot::new(Vec::new())) })
    }
}

fn catalog() -> Catalog {
    let response: ModelsResponse =
        serde_json::from_str(include_str!("fixtures/models.json")).expect("valid model fixture");
    Catalog::from_codex(response.models).expect("compatible GPT catalog")
}

async fn gateway() -> (claude_gpt::gateway::RunningGateway, Arc<MockTransport>) {
    let transport = Arc::new(MockTransport::default());
    let state = SessionState::new(
        transport.clone(),
        Arc::new(catalog()),
        "session-secret".to_string(),
    );
    let gateway = Gateway::bind(state).await.expect("bind loopback gateway");
    (gateway, transport)
}

#[tokio::test]
async fn requires_the_session_bearer_on_every_private_route() {
    let (gateway, _) = gateway().await;
    let client = Client::new();

    for path in ["/v1/models", "/codex/usage"] {
        assert_eq!(
            client.get(gateway.url(path)).send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        assert_eq!(
            client
                .post(gateway.url(path))
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        client
            .get(gateway.url("/v1/models"))
            .bearer_auth(gateway.token())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn discovers_exact_gpt_names_and_context_windows() {
    let (gateway, _) = gateway().await;
    let response: Value = Client::new()
        .get(gateway.url("/v1/models?limit=1000"))
        .bearer_auth(gateway.token())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        response["data"][0]["id"],
        "claude-gpt-openai::gpt-5.6-sol::872000[1m]"
    );
    assert_eq!(response["data"][0]["display_name"], "GPT-5.6-Sol [872K]");
    assert_eq!(response["data"][0]["max_input_tokens"], 828_400);
    assert_eq!(
        response["data"][0]["capabilities"]["effort"]["max"]["supported"],
        true
    );
    assert_eq!(response["has_more"], false);

    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn counts_with_the_same_estimator_used_by_preflight() {
    let (gateway, _) = gateway().await;
    let request: Value =
        serde_json::from_str(include_str!("fixtures/anthropic_mixed_request.json"))
            .expect("valid Anthropic request");
    let response: Value = Client::new()
        .post(gateway.url("/v1/messages/count_tokens"))
        .bearer_auth(gateway.token())
        .json(&request)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert!(response["input_tokens"].as_u64().unwrap() > 0);

    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn streams_a_tool_call_and_maps_max_to_ultra() {
    let (gateway, transport) = gateway().await;
    let mut request: Value =
        serde_json::from_str(include_str!("fixtures/anthropic_mixed_request.json")).unwrap();
    request["output_config"] = json!({"effort": "max"});
    let response = Client::new()
        .post(gateway.url("/v1/messages"))
        .bearer_auth(gateway.token())
        .json(&request)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let body = response.text().await.unwrap();
    assert!(body.contains("event: content_block_start"));
    assert!(body.contains("\"type\":\"tool_use\""));
    assert!(body.contains("event: message_stop"));
    assert_eq!(
        transport.requests.lock().unwrap()[0]["reasoning"]["effort"],
        "xhigh"
    );

    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn returns_a_message_json_for_a_non_streaming_request() {
    let (gateway, _) = gateway().await;
    let mut request: Value =
        serde_json::from_str(include_str!("fixtures/anthropic_mixed_request.json")).unwrap();
    request["stream"] = json!(false);

    let response = Client::new()
        .post(gateway.url("/v1/messages"))
        .bearer_auth(gateway.token())
        .json(&request)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/json");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["stop_reason"], "tool_use");
    assert_eq!(
        body["content"][0],
        json!({"type": "text", "text": "Checking the weather."})
    );
    assert_eq!(body["content"][1]["type"], "tool_use");
    assert_eq!(body["content"][1]["name"], "weather");
    assert_eq!(body["content"][1]["input"], json!({"city": "Paris"}));

    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn promotes_an_overflowing_resumed_window_before_request_preflight() {
    let (gateway, _) = gateway().await;
    let mut request: Value =
        serde_json::from_str(include_str!("fixtures/anthropic_mixed_request.json")).unwrap();
    request["model"] = json!("claude-gpt-openai::gpt-5.6-terra::272000");
    request["messages"] = json!([{
        "role": "user",
        "content": "abcd".repeat(300_000)
    }]);
    request["tools"] = json!([]);
    request["tool_choice"] = Value::Null;

    let response = Client::new()
        .post(gateway.url("/v1/messages"))
        .bearer_auth(gateway.token())
        .json(&request)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await.unwrap().contains("message_stop"));

    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn accepts_image_history_larger_than_axums_default_body_limit() {
    let (gateway, _) = gateway().await;
    let mut request: Value =
        serde_json::from_str(include_str!("fixtures/anthropic_mixed_request.json")).unwrap();
    request["messages"] = json!([{
        "role": "user",
        "content": [{
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/jpeg",
                "data": "A".repeat(2_100_000)
            }
        }]
    }]);
    request["tools"] = json!([]);
    request["tool_choice"] = Value::Null;

    let response = Client::new()
        .post(gateway.url("/v1/messages"))
        .bearer_auth(gateway.token())
        .json(&request)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await.unwrap().contains("message_stop"));

    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn binds_concurrent_sessions_to_distinct_loopback_ports() {
    let (first, _) = gateway().await;
    let (second, _) = gateway().await;

    assert_ne!(first.base_url(), second.base_url());
    assert!(first.base_url().starts_with("http://127.0.0.1:"));
    assert!(second.base_url().starts_with("http://127.0.0.1:"));

    first.shutdown().await.unwrap();
    second.shutdown().await.unwrap();
}

#[tokio::test]
async fn returns_marked_codex_usage_without_upstream_call() {
    let (gateway, transport) = gateway().await;
    let mut request: Value =
        serde_json::from_str(include_str!("fixtures/anthropic_mixed_request.json")).unwrap();
    request["messages"] = json!([{
        "role": "user",
        "content": "<claude-gpt-usage>\nCodex subscription usage\n  primary: 100%\n</claude-gpt-usage>"
    }]);
    request["tools"] = json!([]);
    request["tool_choice"] = Value::Null;

    let response = Client::new()
        .post(gateway.url("/v1/messages"))
        .bearer_auth(gateway.token())
        .json(&request)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let body = response.text().await.unwrap();
    assert!(body.contains("Codex subscription usage"));
    assert!(body.contains("message_start"));
    assert!(body.contains("message_stop"));
    assert_eq!(transport.requests.lock().unwrap().len(), 0);

    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn returns_usage_when_the_client_wraps_the_rendered_command() {
    let (gateway, transport) = gateway().await;
    let mut request: Value =
        serde_json::from_str(include_str!("fixtures/anthropic_mixed_request.json")).unwrap();
    request["messages"] = json!([{
        "role": "user",
        "content": "Use the following command result directly.\n<claude-gpt-usage>\nCodex subscription usage\n  primary: 86% remaining\n</claude-gpt-usage>\nDo not transform it."
    }]);
    request["tools"] = json!([]);
    request["tool_choice"] = Value::Null;

    let response = Client::new()
        .post(gateway.url("/v1/messages"))
        .bearer_auth(gateway.token())
        .json(&request)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await.unwrap().contains("86% remaining"));
    assert_eq!(transport.requests.lock().unwrap().len(), 0);

    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn returns_usage_before_claude_can_turn_the_command_into_a_skill_call() {
    let (gateway, transport) = gateway().await;
    let mut request: Value =
        serde_json::from_str(include_str!("fixtures/anthropic_mixed_request.json")).unwrap();
    request["messages"] = json!([{
        "role": "user",
        "content": "<command-name>/codex-usage</command-name>\n<command-message>/codex-usage</command-message>"
    }]);
    request["tools"] = json!([]);
    request["tool_choice"] = Value::Null;

    let response = Client::new()
        .post(gateway.url("/v1/messages"))
        .bearer_auth(gateway.token())
        .json(&request)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("Codex subscription usage")
    );
    assert_eq!(transport.requests.lock().unwrap().len(), 0);

    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn redacts_upstream_details_and_returns_a_correlation_id() {
    let state = SessionState::new(
        Arc::new(ErrorTransport),
        Arc::new(catalog()),
        "session-secret".to_string(),
    );
    let gateway = Gateway::bind(state).await.unwrap();
    let request: Value =
        serde_json::from_str(include_str!("fixtures/anthropic_mixed_request.json")).unwrap();
    let response = Client::new()
        .post(gateway.url("/v1/messages"))
        .bearer_auth(gateway.token())
        .json(&request)
        .send()
        .await
        .unwrap();
    let body = response.text().await.unwrap();

    assert!(body.contains("upstream request failed"));
    assert!(body.contains("correlation id:"));
    assert!(!body.contains("oauth-secret"));
    assert!(!body.contains("tool payload"));

    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn drops_the_upstream_stream_when_the_client_disconnects() {
    let dropped = Arc::new(AtomicBool::new(false));
    let state = SessionState::new(
        Arc::new(DisconnectTransport {
            dropped: dropped.clone(),
        }),
        Arc::new(catalog()),
        "session-secret".to_string(),
    );
    let gateway = Gateway::bind(state).await.unwrap();
    let request: Value =
        serde_json::from_str(include_str!("fixtures/anthropic_mixed_request.json")).unwrap();
    let response = Client::new()
        .post(gateway.url("/v1/messages"))
        .bearer_auth(gateway.token())
        .json(&request)
        .send()
        .await
        .unwrap();
    let mut body = response.bytes_stream();
    body.next().await.expect("first SSE chunk").unwrap();
    drop(body);

    for _ in 0..50 {
        if dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(dropped.load(Ordering::SeqCst));

    gateway.shutdown().await.unwrap();
}
