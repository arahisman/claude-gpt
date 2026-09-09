use std::path::Path;
use std::process::Command;

use codex_api::ResponseEvent;
use futures::StreamExt;
use serde_json::Value;

use crate::anthropic::AnthropicRequest;
use crate::codex_transport::CodexTransport;
use crate::compatibility::Compatibility;
use crate::config_patch::read_jsonc_string;
use crate::effort::{ClaudeEffort, resolve_effort};
use crate::error::{BridgeError, Result};
use crate::paths::AppPaths;
use crate::responses::convert_request;

const WRAPPER_KEY: &str = "claudeCode.claudeProcessWrapper";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorMode {
    Offline,
    Live,
}

impl DoctorMode {
    pub fn runs_inference(self) -> bool {
        self == Self::Live
    }
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub checks: Vec<String>,
}

impl std::fmt::Display for DoctorReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(formatter, "claude-gpt doctor: OK")?;
        for check in &self.checks {
            writeln!(formatter, "  ✓ {check}")?;
        }
        Ok(())
    }
}

pub async fn run(paths: &AppPaths, mode: DoctorMode) -> Result<DoctorReport> {
    let verified = Compatibility::embedded()?.verify(paths)?;
    let help = command_stdout(&verified.claude_cli, &["--help"])?;
    check_claude_help(&help)?;
    let package_bytes =
        std::fs::read(&verified.vscode_package_json).map_err(|source| BridgeError::Read {
            path: verified.vscode_package_json.clone(),
            source,
        })?;
    let package: Value =
        serde_json::from_slice(&package_bytes).map_err(|source| BridgeError::ParseJson {
            path: verified.vscode_package_json.clone(),
            source,
        })?;
    check_extension_schema(&package)?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|error| BridgeError::CodexTransport(format!("loopback bind failed: {error}")))?;
    drop(listener);
    check_installed_files(paths)?;

    let transport = CodexTransport::connect(paths).await?;
    let catalog = transport.refresh_catalog().await?;
    let mut checks = vec![
        format!("Claude CLI at {}", verified.claude_cli.display()),
        format!("VS Code extension at {}", paths.vscode_extension.display()),
        format!(
            "VS Code embedded Claude at {}",
            verified.vscode_claude.display()
        ),
        format!(
            "Codex with ChatGPT authentication at {}",
            verified.codex.display()
        ),
        "loopback gateway bind".to_string(),
        format!("{} GPT model variants discovered", catalog.visible().len()),
        "VS Code wrapper and /codex-usage command".to_string(),
    ];
    if mode.runs_inference() {
        run_live_turn(&transport, &catalog).await?;
        checks.push("minimal live inference".to_string());
    }
    Ok(DoctorReport { checks })
}

pub fn check_claude_help(help: &str) -> Result<()> {
    for required in [
        "--settings",
        "--continue",
        "--resume",
        "--effort <level>",
        "low, medium, high, xhigh, max",
    ] {
        if !help.contains(required) {
            return Err(BridgeError::Compatibility(format!(
                "Claude CLI help is missing required contract {required:?}"
            )));
        }
    }
    Ok(())
}

pub fn check_extension_schema(package: &Value) -> Result<()> {
    if package
        .pointer("/contributes/configuration/properties/claudeCode.claudeProcessWrapper/type")
        .and_then(Value::as_str)
        == Some("string")
    {
        Ok(())
    } else {
        Err(BridgeError::Compatibility(
            "VS Code extension does not expose string setting claudeCode.claudeProcessWrapper"
                .to_string(),
        ))
    }
}

fn check_installed_files(paths: &AppPaths) -> Result<()> {
    let settings =
        std::fs::read_to_string(&paths.vscode_settings).map_err(|source| BridgeError::Read {
            path: paths.vscode_settings.clone(),
            source,
        })?;
    let expected = paths.install_bin.to_str().ok_or_else(|| {
        BridgeError::InvalidConfig(format!(
            "installed binary path is not UTF-8: {}",
            paths.install_bin.display()
        ))
    })?;
    if read_jsonc_string(&settings, WRAPPER_KEY)?.as_deref() != Some(expected) {
        return Err(BridgeError::Compatibility(format!(
            "VS Code setting {WRAPPER_KEY} is not installed"
        )));
    }
    for path in [
        &paths.install_bin,
        &paths.install_manifest,
        &paths.claude_commands.join("codex-usage.md"),
    ] {
        if !path.is_file() {
            return Err(BridgeError::Compatibility(format!(
                "required installed file is missing: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

async fn run_live_turn(
    transport: &CodexTransport,
    catalog: &crate::catalog::Catalog,
) -> Result<()> {
    let model = catalog.default();
    let request: AnthropicRequest = serde_json::from_value(serde_json::json!({
        "model": model.gateway_id,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": "Reply with OK only."}],
        "stream": true
    }))
    .map_err(|source| BridgeError::InvalidRequest {
        path: "doctor.live".to_string(),
        message: source.to_string(),
    })?;
    let effort = resolve_effort(
        ClaudeEffort::Low,
        &model.supported_efforts,
        model.default_effort.clone(),
    );
    let body = convert_request(&request, model, effort)?;
    let mut stream = transport.stream(body).await?;
    while let Some(event) = stream.next().await {
        if matches!(event, Ok(ResponseEvent::Completed { .. })) {
            return Ok(());
        }
        event.map_err(|error| BridgeError::CodexTransport(error.to_string()))?;
    }
    Err(BridgeError::InvalidStream(
        "live doctor stream ended before response.completed".to_string(),
    ))
}

fn command_stdout(program: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .map_err(|source| BridgeError::Process {
            program: program.to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        return Err(BridgeError::Compatibility(format!(
            "{} returned {}",
            program.display(),
            output.status
        )));
    }
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}
