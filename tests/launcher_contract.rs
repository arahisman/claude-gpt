use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use claude_gpt::catalog::Catalog;
use claude_gpt::gateway::{Gateway, GatewayEventStream, GatewayTransport, SessionState};
use claude_gpt::launcher::{SessionSettings, launch_with_gateway, select_claude_invocation};
use claude_gpt::usage::UsageSnapshot;
use codex_protocol::openai_models::ModelsResponse;
use serde_json::Value;

struct IdleTransport;

impl GatewayTransport for IdleTransport {
    fn stream(
        &self,
        _body: Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<GatewayEventStream, String>> + Send + '_>,
    > {
        Box::pin(async { Err("unused".to_string()) })
    }

    fn rate_limits(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<UsageSnapshot, String>> + Send + '_>,
    > {
        Box::pin(async { Ok(UsageSnapshot::new(Vec::new())) })
    }
}

fn catalog() -> Catalog {
    let response: ModelsResponse =
        serde_json::from_str(include_str!("fixtures/models.json")).expect("valid model fixture");
    Catalog::from_codex(response.models).expect("compatible GPT catalog")
}

fn fake_claude() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_claude.sh")
}

async fn gateway() -> claude_gpt::gateway::RunningGateway {
    Gateway::bind(SessionState::new(
        Arc::new(IdleTransport),
        Arc::new(catalog()),
        "launcher-secret".to_string(),
    ))
    .await
    .expect("bind gateway")
}

#[test]
fn consumes_the_vscode_embedded_cli_path_but_preserves_sdk_arguments() {
    let standalone = Path::new("/pinned/standalone/claude");
    let embedded = Path::new("/pinned/extension/claude");
    let (executable, arguments) = select_claude_invocation(
        standalone,
        embedded,
        vec![
            embedded.as_os_str().into(),
            "--resume".into(),
            "session-id".into(),
        ],
    );

    assert_eq!(executable, embedded);
    assert_eq!(arguments, vec!["--resume", "session-id"]);

    let (executable, arguments) =
        select_claude_invocation(standalone, embedded, vec!["--continue".into()]);
    assert_eq!(executable, standalone);
    assert_eq!(arguments, vec!["--continue"]);
}

#[test]
fn writes_private_session_settings_with_dynamic_models_and_default() {
    let directory = tempfile::tempdir().unwrap();
    let settings = SessionSettings::create(directory.path(), &catalog()).unwrap();
    let metadata = fs::metadata(settings.path()).unwrap();
    let value: Value = serde_json::from_slice(&fs::read(settings.path()).unwrap()).unwrap();

    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(value["modelDiscoveryEnabled"], true);
    assert_eq!(value["autoCompactEnabled"], true);
    assert_eq!(
        value["model"],
        "claude-gpt-openai::gpt-5.6-terra::872000[1m]"
    );
    assert_eq!(
        value["inferenceModels"][0]["name"],
        "claude-gpt-openai::gpt-5.6-sol::872000[1m]"
    );
    assert_eq!(
        value["inferenceModels"][0]["labelOverride"],
        "GPT-5.6-Sol [872K]"
    );
    assert_eq!(
        value["inferenceModels"][1]["name"],
        "claude-gpt-openai::gpt-5.6-sol::272000"
    );
    assert_eq!(value["autoCompactWindow"], 784_800);
    assert_eq!(value["modelPicker"]["replaceBuiltInOptions"], true);
    assert_eq!(
        value["modelPicker"]["options"][0]["model"],
        "claude-gpt-openai::gpt-5.6-sol::872000[1m]"
    );
    assert_eq!(
        value["modelPicker"]["options"][0]["label"],
        "GPT-5.6-Sol [872K]"
    );
    assert_eq!(
        value["modelPicker"]["options"][0]["behavesAs"],
        "claude-opus-4-8[1m]"
    );
    assert_eq!(
        value["modelPicker"]["options"][1]["behavesAs"],
        "claude-opus-4-8"
    );

    let path = settings.path().to_path_buf();
    drop(settings);
    assert!(!path.exists());
}

#[tokio::test]
async fn preserves_claude_arguments_sets_gateway_environment_and_cleans_up() {
    let directory = tempfile::tempdir().unwrap();
    let capture = directory.path().join("capture.txt");
    let gateway = gateway().await;
    let base_url = gateway.base_url();
    let health_url = gateway.url("/healthz");

    let status = launch_with_gateway(
        &fake_claude(),
        vec!["--resume".into(), "session-id".into(), "hello world".into()],
        &catalog(),
        directory.path(),
        gateway,
        &[("FAKE_CLAUDE_CAPTURE".into(), capture.as_os_str().into())],
    )
    .await
    .unwrap();

    assert!(status.success());
    let output = fs::read_to_string(capture).unwrap();
    let lines = output.lines().collect::<Vec<_>>();
    assert_eq!(
        &lines[0..3],
        &["arg=--resume", "arg=session-id", "arg=hello world"]
    );
    assert_eq!(lines[3], "arg=--settings");
    let settings_path = lines[4].strip_prefix("arg=").unwrap();
    assert!(!Path::new(settings_path).exists());
    assert_eq!(lines[5], "arg=--mcp-config");
    let mcp_config_path = lines[6].strip_prefix("arg=").unwrap();
    assert!(!Path::new(mcp_config_path).exists());
    assert!(output.contains(&format!("base_url={base_url}")));
    assert!(output.contains("auth_token=launcher-secret"));
    assert!(output.contains("use_gateway=1"));
    assert!(output.contains("model_discovery=1"));
    assert!(output.contains("allow_loopback=1"));
    assert!(output.contains("api_key="));
    assert!(output.contains("oauth_token="));
    assert!(reqwest::get(health_url).await.is_err());
}

#[tokio::test]
async fn returns_the_exact_claude_exit_status() {
    let directory = tempfile::tempdir().unwrap();
    let capture = directory.path().join("capture.txt");
    fs::set_permissions(fake_claude(), fs::Permissions::from_mode(0o755)).unwrap();

    let status = launch_with_gateway(
        &fake_claude(),
        Vec::new(),
        &catalog(),
        directory.path(),
        gateway().await,
        &[
            ("FAKE_CLAUDE_CAPTURE".into(), capture.as_os_str().into()),
            ("FAKE_CLAUDE_EXIT_CODE".into(), "37".into()),
        ],
    )
    .await
    .unwrap();

    assert_eq!(status.code(), Some(37));
}
