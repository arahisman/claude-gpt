use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::process::{Child, Command};
use tokio::signal::unix::{SignalKind, signal};

use crate::catalog::{Catalog, load_cached_catalog};
use crate::codex_transport::CodexTransport;
use crate::compatibility::Compatibility;
use crate::error::{BridgeError, Result};
use crate::gateway::{Gateway, RunningGateway, SessionState};
use crate::paths::AppPaths;

pub struct SessionSettings {
    file: NamedTempFile,
}

struct McpConfig {
    file: NamedTempFile,
}

pub async fn run(args: Vec<OsString>) -> Result<ExitStatus> {
    let paths = AppPaths::resolve()?;
    let verified = Compatibility::embedded()?.verify(&paths)?;
    let transport = std::sync::Arc::new(CodexTransport::connect(&paths).await?);
    let catalog = match transport.refresh_catalog().await {
        Ok(catalog) => catalog,
        Err(live_error) => match load_cached_catalog(&paths.catalog_cache) {
            Ok(catalog) => {
                eprintln!(
                    "claude-gpt: live model discovery failed; using the last verified catalog: {live_error}"
                );
                catalog
            }
            Err(cache_error) => {
                return Err(BridgeError::CodexTransport(format!(
                    "live model discovery failed ({live_error}) and no usable cached catalog exists ({cache_error})"
                )));
            }
        },
    };
    let catalog = std::sync::Arc::new(catalog);
    let gateway =
        Gateway::bind(SessionState::with_random_token(transport, catalog.clone())?).await?;
    let (claude_cli, args) = select_claude_invocation(
        &verified.claude_cli.path,
        &verified.vscode_claude.path,
        args,
    );
    launch_with_gateway(&claude_cli, args, &catalog, &paths.state_dir, gateway, &[]).await
}

pub fn select_claude_invocation(
    standalone_cli: &Path,
    vscode_cli: &Path,
    mut args: Vec<OsString>,
) -> (PathBuf, Vec<OsString>) {
    if args.first().is_some_and(|value| value == vscode_cli) {
        args.remove(0);
        (vscode_cli.to_path_buf(), args)
    } else {
        (standalone_cli.to_path_buf(), args)
    }
}

impl SessionSettings {
    pub fn create(directory: &Path, catalog: &Catalog) -> Result<Self> {
        std::fs::create_dir_all(directory).map_err(|source| BridgeError::Write {
            path: directory.to_path_buf(),
            source,
        })?;
        let mut file = tempfile::Builder::new()
            .prefix("session-")
            .suffix(".json")
            .tempfile_in(directory)
            .map_err(|source| BridgeError::Write {
                path: directory.to_path_buf(),
                source,
            })?;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|source| BridgeError::Write {
                path: file.path().to_path_buf(),
                source,
            })?;
        let value = session_settings(catalog);
        serde_json::to_writer_pretty(&mut file, &value).map_err(|source| {
            BridgeError::SerializeJson {
                path: file.path().to_path_buf(),
                source,
            }
        })?;
        file.flush().map_err(|source| BridgeError::Write {
            path: file.path().to_path_buf(),
            source,
        })?;
        file.as_file()
            .sync_all()
            .map_err(|source| BridgeError::Write {
                path: file.path().to_path_buf(),
                source,
            })?;
        Ok(Self { file })
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }
}

impl McpConfig {
    fn create(directory: &Path) -> Result<Self> {
        let executable = std::env::current_exe().map_err(|source| BridgeError::Process {
            program: "claude-gpt".into(),
            source,
        })?;
        let mut file = tempfile::Builder::new()
            .prefix("mcp-")
            .suffix(".json")
            .tempfile_in(directory)
            .map_err(|source| BridgeError::Write {
                path: directory.to_path_buf(),
                source,
            })?;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|source| BridgeError::Write {
                path: file.path().to_path_buf(),
                source,
            })?;
        serde_json::to_writer(
            &mut file,
            &json!({
                "mcpServers": {
                    "claude-gpt-imagegen": {
                        "type": "stdio",
                        "command": executable,
                        "args": ["mcp"],
                        "env": {}
                    }
                }
            }),
        )
        .map_err(|source| BridgeError::SerializeJson {
            path: file.path().to_path_buf(),
            source,
        })?;
        file.flush().map_err(|source| BridgeError::Write {
            path: file.path().to_path_buf(),
            source,
        })?;
        file.as_file()
            .sync_all()
            .map_err(|source| BridgeError::Write {
                path: file.path().to_path_buf(),
                source,
            })?;
        Ok(Self { file })
    }

    fn path(&self) -> &Path {
        self.file.path()
    }
}

pub async fn launch_with_gateway(
    claude_cli: &Path,
    args: Vec<OsString>,
    catalog: &Catalog,
    state_dir: &Path,
    gateway: RunningGateway,
    extra_environment: &[(OsString, OsString)],
) -> Result<ExitStatus> {
    let settings = SessionSettings::create(state_dir, catalog)?;
    let mcp_config = McpConfig::create(state_dir)?;
    gateway.ready().await?;
    let base_url = gateway.base_url();
    let token = gateway.token().to_owned();
    let mut command = Command::new(claude_cli);
    command
        .args(args)
        .arg("--settings")
        .arg(settings.path())
        .arg("--mcp-config")
        .arg(mcp_config.path())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .envs(extra_environment.iter().cloned())
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_AUTH_TOKEN", token)
        .env("CLAUDE_CODE_USE_GATEWAY", "1")
        .env("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1")
        .env("CLAUDE_GATEWAY_ALLOW_LOOPBACK", "1")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN");

    let child_result = command.spawn().map_err(|source| BridgeError::Process {
        program: claude_cli.to_path_buf(),
        source,
    });
    let status_result = match child_result {
        Ok(mut child) => wait_for_child(claude_cli, &mut child).await,
        Err(error) => Err(error),
    };
    let shutdown_result = gateway.shutdown().await;
    drop(settings);
    drop(mcp_config);

    match (status_result, shutdown_result) {
        (Ok(status), Ok(())) => Ok(status),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

async fn wait_for_child(program: &Path, child: &mut Child) -> Result<ExitStatus> {
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|source| BridgeError::Process {
        program: program.to_path_buf(),
        source,
    })?;
    let mut terminate = signal(SignalKind::terminate()).map_err(|source| BridgeError::Process {
        program: program.to_path_buf(),
        source,
    })?;

    loop {
        tokio::select! {
            status = child.wait() => {
                return status.map_err(|source| BridgeError::Process {
                    program: program.to_path_buf(),
                    source,
                });
            }
            signal = interrupt.recv() => {
                if signal.is_some() {
                    forward_signal(child, libc::SIGINT);
                }
            }
            signal = terminate.recv() => {
                if signal.is_some() {
                    forward_signal(child, libc::SIGTERM);
                }
            }
        }
    }
}

fn forward_signal(child: &Child, signal: libc::c_int) {
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(pid as libc::pid_t, signal);
        }
    }
}

fn session_settings(catalog: &Catalog) -> Value {
    let inference_models = catalog
        .visible()
        .iter()
        .map(|model| {
            json!({
                "name": model.gateway_id,
                "labelOverride": model.display_name,
            })
        })
        .collect::<Vec<_>>();
    let model_picker = catalog
        .visible()
        .iter()
        .map(|model| {
            json!({
                "model": model.gateway_id,
                "label": model.display_name,
                "description": format!(
                    "ChatGPT subscription · {} usable tokens",
                    model.usable_tokens
                ),
                "behavesAs": if model.extended {
                    "claude-opus-4-8[1m]"
                } else {
                    "claude-opus-4-8"
                },
            })
        })
        .collect::<Vec<_>>();
    json!({
        "autoCompactEnabled": true,
        "autoCompactWindow": catalog.default().auto_compact_tokens.clamp(100_000, 1_000_000),
        "inferenceModels": inference_models,
        "model": catalog.default().gateway_id,
        "modelDiscoveryEnabled": true,
        "modelPicker": {
            "options": model_picker,
            "replaceBuiltInOptions": true,
        },
    })
}

pub fn settings_path_from_args(args: &[OsString]) -> Option<PathBuf> {
    args.windows(2)
        .find(|pair| pair[0] == "--settings")
        .map(|pair| PathBuf::from(&pair[1]))
}
