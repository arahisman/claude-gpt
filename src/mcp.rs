use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::codex_transport::CodexTransport;
use crate::error::{BridgeError, Result};
use crate::paths::AppPaths;

const SERVER_NAME: &str = "claude-gpt-imagegen";
const IMAGEGEN_TOOL_NAME: &str = "imagegen";
const IMAGEEDIT_TOOL_NAME: &str = "imageedit";
const MAX_REFERENCE_IMAGES: usize = 5;

pub trait ImageGenerator: Send + Sync {
    fn generate_image<'a>(
        &'a self,
        prompt: String,
        referenced_image_paths: Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<Vec<u8>, String>> + Send + 'a>>;
}

impl<T> ImageGenerator for &T
where
    T: ImageGenerator + ?Sized,
{
    fn generate_image<'a>(
        &'a self,
        prompt: String,
        referenced_image_paths: Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<Vec<u8>, String>> + Send + 'a>> {
        (*self).generate_image(prompt, referenced_image_paths)
    }
}

impl ImageGenerator for CodexTransport {
    fn generate_image<'a>(
        &'a self,
        prompt: String,
        referenced_image_paths: Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<Vec<u8>, String>> + Send + 'a>> {
        Box::pin(async move {
            CodexTransport::generate_image(self, prompt, referenced_image_paths)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

pub struct McpServer<G> {
    image_generator: G,
    project_dir: PathBuf,
}

impl<G> McpServer<G>
where
    G: ImageGenerator,
{
    pub fn for_project(image_generator: G, project_dir: PathBuf) -> Self {
        Self {
            image_generator,
            project_dir,
        }
    }

    pub async fn handle_request(&self, request: Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let response = match request.get("method").and_then(Value::as_str) {
            Some("initialize") => success(
                id.clone(),
                json!({
                    "protocolVersion": request
                        .pointer("/params/protocolVersion")
                        .and_then(Value::as_str)
                        .unwrap_or("2025-06-18"),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")}
                }),
            ),
            Some("tools/list") => success(id.clone(), tools_list()),
            Some("tools/call") => self.call_tool(id.clone(), request.get("params")).await,
            Some(_) => failure(id.clone(), -32601, "method not found"),
            None => failure(id.clone(), -32600, "request is missing method"),
        };
        id.map(|_| response)
    }

    async fn call_tool(&self, id: Option<Value>, params: Option<&Value>) -> Value {
        let Some(params) = params else {
            return failure(id, -32602, "tools/call requires params");
        };
        let tool_name = match params.get("name").and_then(Value::as_str) {
            Some(IMAGEGEN_TOOL_NAME) => IMAGEGEN_TOOL_NAME,
            Some(IMAGEEDIT_TOOL_NAME) => IMAGEEDIT_TOOL_NAME,
            _ => return tool_failure(id, "unknown tool"),
        };
        let arguments = params.get("arguments").unwrap_or(&Value::Null);
        let prompt = match arguments.get("prompt").and_then(Value::as_str) {
            Some(prompt) if !prompt.trim().is_empty() => prompt.to_string(),
            _ => return failure(id, -32602, "image generation requires a non-empty prompt"),
        };
        let reference_paths = match reference_paths(arguments, &self.project_dir) {
            Ok(paths) => paths,
            Err(error) => return failure(id, -32602, &error),
        };
        match tool_name {
            IMAGEGEN_TOOL_NAME if !reference_paths.is_empty() => {
                return failure(
                    id,
                    -32602,
                    "imagegen does not accept reference images; use imageedit instead",
                );
            }
            IMAGEEDIT_TOOL_NAME if reference_paths.is_empty() => {
                return failure(
                    id,
                    -32602,
                    "imageedit requires at least one reference image",
                );
            }
            _ => {}
        }
        match self
            .image_generator
            .generate_image(prompt, reference_paths)
            .await
        {
            Ok(image) => match save_image(&self.project_dir, &image) {
                Ok(path) => success(
                    id,
                    json!({"content": [{"type": "text", "text": format!("Generated image: {}", path.display())}]}),
                ),
                Err(error) => tool_failure(id, &error.to_string()),
            },
            Err(error) => tool_failure(id, &error),
        }
    }
}

pub async fn run(paths: AppPaths) -> Result<()> {
    let project_dir = std::env::var_os("CLAUDE_PROJECT_DIR")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir().map_err(|source| BridgeError::Read {
            path: PathBuf::from("."),
            source,
        })?);
    let generator = LazyImageGenerator::new(paths);
    let server = McpServer::for_project(generator, project_dir);
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|source| BridgeError::Read {
            path: PathBuf::from("stdin"),
            source,
        })?
    {
        let request = match serde_json::from_str::<Value>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut stdout,
                    failure(Some(Value::Null), -32700, &error.to_string()),
                )
                .await?;
                continue;
            }
        };
        if let Some(response) = server.handle_request(request).await {
            write_response(&mut stdout, response).await?;
        }
    }
    Ok(())
}

struct LazyImageGenerator {
    paths: AppPaths,
    transport: Mutex<Option<Arc<CodexTransport>>>,
}

impl LazyImageGenerator {
    fn new(paths: AppPaths) -> Self {
        Self {
            paths,
            transport: Mutex::new(None),
        }
    }

    async fn transport(&self) -> std::result::Result<Arc<CodexTransport>, String> {
        let mut transport = self.transport.lock().await;
        if let Some(transport) = transport.as_ref() {
            return Ok(transport.clone());
        }
        let connected = Arc::new(
            CodexTransport::connect(&self.paths)
                .await
                .map_err(|error| error.to_string())?,
        );
        *transport = Some(connected.clone());
        Ok(connected)
    }
}

impl ImageGenerator for LazyImageGenerator {
    fn generate_image<'a>(
        &'a self,
        prompt: String,
        referenced_image_paths: Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<Vec<u8>, String>> + Send + 'a>> {
        Box::pin(async move {
            self.transport()
                .await?
                .generate_image(prompt, referenced_image_paths)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": IMAGEGEN_TOOL_NAME,
                "description": "Create a new PNG image from a text prompt through the ChatGPT Codex subscription. Returns only the saved local path.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string", "description": "Text-to-image instruction."}
                    },
                    "required": ["prompt"],
                    "additionalProperties": false
                }
            },
            {
                "name": IMAGEEDIT_TOOL_NAME,
                "description": "Create a new PNG image from one or more reference images and a text instruction. Never overwrites the reference images. Returns only the saved local path.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string", "description": "Image editing instruction."},
                        "referenced_image_paths": {"type": "array", "minItems": 1, "maxItems": MAX_REFERENCE_IMAGES, "items": {"type": "string"}}
                    },
                    "required": ["prompt", "referenced_image_paths"],
                    "additionalProperties": false
                }
            }
        ]
    })
}

fn reference_paths(
    arguments: &Value,
    project_dir: &Path,
) -> std::result::Result<Vec<PathBuf>, String> {
    let Some(paths) = arguments.get("referenced_image_paths") else {
        return Ok(Vec::new());
    };
    let paths = paths
        .as_array()
        .ok_or_else(|| "referenced_image_paths must be an array of paths".to_string())?;
    if paths.len() > MAX_REFERENCE_IMAGES {
        return Err(format!(
            "referenced_image_paths supports at most {MAX_REFERENCE_IMAGES} images"
        ));
    }
    paths
        .iter()
        .map(|path| {
            let path = path
                .as_str()
                .ok_or_else(|| "referenced_image_paths must contain only strings".to_string())?;
            let path = PathBuf::from(path);
            Ok(if path.is_absolute() {
                path
            } else {
                project_dir.join(path)
            })
        })
        .collect()
}

fn save_image(project_dir: &Path, image: &[u8]) -> Result<PathBuf> {
    let directory = project_dir.join("generated_images").join("claude-gpt");
    std::fs::create_dir_all(&directory).map_err(|source| BridgeError::Write {
        path: directory.clone(),
        source,
    })?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| BridgeError::InvalidConfig(format!("system clock error: {error}")))?
        .as_nanos();
    for suffix in 0_u16..1000 {
        let path = directory.join(format!("image-{stamp}-{suffix}.png"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(image).map_err(|source| BridgeError::Write {
                    path: path.clone(),
                    source,
                })?;
                file.sync_all().map_err(|source| BridgeError::Write {
                    path: path.clone(),
                    source,
                })?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(BridgeError::Write { path, source }),
        }
    }
    Err(BridgeError::Write {
        path: directory,
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate image filename",
        ),
    })
}

async fn write_response(stdout: &mut tokio::io::Stdout, response: Value) -> Result<()> {
    let mut line = serde_json::to_vec(&response)
        .map_err(|error| BridgeError::InvalidConfig(error.to_string()))?;
    line.push(b'\n');
    stdout
        .write_all(&line)
        .await
        .map_err(|source| BridgeError::Write {
            path: PathBuf::from("stdout"),
            source,
        })?;
    stdout.flush().await.map_err(|source| BridgeError::Write {
        path: PathBuf::from("stdout"),
        source,
    })
}

fn success(id: Option<Value>, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result})
}

fn failure(id: Option<Value>, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "error": {"code": code, "message": message}})
}

fn tool_failure(id: Option<Value>, message: &str) -> Value {
    success(
        id,
        json!({"content": [{"type": "text", "text": message}], "isError": true}),
    )
}
