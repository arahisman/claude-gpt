use std::fs;

use claude_gpt::doctor::{DoctorMode, check_claude_help, check_extension_schema};
use claude_gpt::logging::{MetadataEvent, MetadataLogger, redact_text};
use claude_gpt::usage::{UsageSnapshot, render_usage};
use codex_protocol::protocol::{CreditsSnapshot, RateLimitSnapshot, RateLimitWindow};

#[test]
fn offline_doctor_never_plans_an_inference_call() {
    assert!(!DoctorMode::Offline.runs_inference());
    assert!(DoctorMode::Live.runs_inference());
}

#[test]
fn validates_the_current_claude_and_extension_contracts() {
    let help = "--settings <file-or-json> --continue --resume --effort <level> (low, medium, high, xhigh, max)";
    assert!(check_claude_help(help).is_ok());
    assert!(check_claude_help("--settings").is_err());

    let package = serde_json::json!({
        "contributes": {"configuration": {"properties": {
            "claudeCode.claudeProcessWrapper": {"type": "string"}
        }}}
    });
    assert!(check_extension_schema(&package).is_ok());
}

#[test]
fn renders_usage_as_a_scannable_markdown_table() {
    let snapshot = UsageSnapshot::new(vec![RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: Some("Codex".to_string()),
        primary: Some(RateLimitWindow {
            used_percent: 12.5,
            window_minutes: Some(300),
            resets_at: Some(1_704_069_000),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 80.0,
            window_minutes: Some(10_080),
            resets_at: None,
        }),
        credits: Some(CreditsSnapshot {
            has_credits: true,
            unlimited: false,
            balance: Some("42.00".to_string()),
        }),
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    }]);

    let output = render_usage(&snapshot);
    assert!(
        output.starts_with(
            "## Codex subscription usage\n\n| Model | Window | Used | Remaining | Resets (UTC) |\n|---|---|---:|---:|---|"
        )
    );
    assert!(output.contains("| Codex | Primary (5h) | 12.5% | **87.5%** | 2024-01-01 00:30 |"));
    assert!(output.contains("| Codex | Secondary (7d) | 80.0% | **20.0%** | unavailable |"));
    assert!(output.ends_with("Credits: **Codex:** 42.00"));
}

#[test]
fn redaction_removes_credentials_and_prompt_like_payloads() {
    let text = "Authorization: Bearer secret-token prompt=private words account_id=acct-123";
    let redacted = redact_text(text);
    assert!(!redacted.contains("secret-token"));
    assert!(!redacted.contains("private words"));
    assert!(!redacted.contains("acct-123"));
}

#[test]
fn metadata_logger_rotates_and_never_accepts_bodies_or_headers() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bridge.log");
    let logger = MetadataLogger::with_limits(path.clone(), 180, 2);
    for index in 0..20 {
        logger
            .write(&MetadataEvent {
                correlation_id: format!("corr-{index}"),
                model_id: Some("gpt-test".to_string()),
                status: Some(200),
                latency_ms: Some(12),
                event_kind: "completed".to_string(),
                input_tokens: Some(3),
                output_tokens: Some(4),
            })
            .unwrap();
    }

    assert!(path.is_file());
    assert!(path.with_extension("log.1").is_file());
    assert!(!path.with_extension("log.3").exists());
    let all = fs::read_to_string(&path).unwrap();
    assert!(!all.contains("Bearer"));
    assert!(!all.contains("prompt"));
}
