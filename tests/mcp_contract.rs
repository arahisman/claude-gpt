use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;

use claude_gpt::mcp::{ImageGenerator, McpServer};
use serde_json::{Value, json};

struct FakeImageGenerator {
    calls: Mutex<Vec<(String, Vec<PathBuf>)>>,
}

impl FakeImageGenerator {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl ImageGenerator for FakeImageGenerator {
    fn generate_image<'a>(
        &'a self,
        prompt: String,
        referenced_image_paths: Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>> {
        self.calls
            .lock()
            .unwrap()
            .push((prompt, referenced_image_paths));
        Box::pin(async { Ok(vec![137, 80, 78, 71]) })
    }
}

#[tokio::test]
async fn exposes_imagegen_and_saves_a_small_mcp_result_outside_the_context() {
    let directory = tempfile::tempdir().unwrap();
    let images = FakeImageGenerator::new();
    let server = McpServer::for_project(&images, directory.path().to_path_buf());

    let initialize = server
        .handle_request(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18"}
        }))
        .await
        .unwrap();
    assert_eq!(initialize["result"]["protocolVersion"], "2025-06-18");

    let tools = server
        .handle_request(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await
        .unwrap();
    assert_eq!(tools["result"]["tools"][0]["name"], "imagegen");
    assert_eq!(
        tools["result"]["tools"][0]["inputSchema"]["required"],
        json!(["prompt"])
    );

    let result = server
        .handle_request(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "imagegen", "arguments": {"prompt": "a blue circle"}}
        }))
        .await
        .unwrap();
    let message = result["result"]["content"][0]["text"].as_str().unwrap();
    let output_path = PathBuf::from(message.strip_prefix("Generated image: ").unwrap());
    assert!(output_path.starts_with(directory.path()));
    assert_eq!(std::fs::read(&output_path).unwrap(), vec![137, 80, 78, 71]);
    assert_eq!(
        images.calls.lock().unwrap().as_slice(),
        [("a blue circle".to_string(), Vec::new())]
    );
    assert!(serde_json::to_string(&result).unwrap().len() < 1_000);
}

#[tokio::test]
async fn rejects_unbounded_or_missing_image_requests() {
    let directory = tempfile::tempdir().unwrap();
    let images = FakeImageGenerator::new();
    let server = McpServer::for_project(&images, directory.path().to_path_buf());

    let response: Value = server
        .handle_request(json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {"name": "imagegen", "arguments": {"referenced_image_paths": ["a", "b", "c", "d", "e", "f"]}}
        }))
        .await
        .unwrap();
    assert_eq!(response["error"]["code"], -32602);
    assert!(images.calls.lock().unwrap().is_empty());
}
