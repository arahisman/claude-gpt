use std::process::Command;

use serde_json::Value;

#[test]
#[ignore = "uses installed Claude and Codex subscription"]
fn installed_doctor_live_and_usage() {
    for arguments in [["doctor", "--live"].as_slice(), ["usage"].as_slice()] {
        let bridge = claude_gpt::paths::AppPaths::resolve()
            .expect("resolve user paths")
            .install_bin;
        let output = Command::new(bridge)
            .args(arguments)
            .output()
            .expect("run installed bridge");
        assert!(
            output.status.success(),
            "{} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.is_empty());
    }
}

#[test]
#[ignore = "uses installed Claude and Codex subscription"]
fn installed_spark_model_completes() {
    let model = "claude-gpt-openai::gpt-5.3-codex-spark::128000";
    let bridge = claude_gpt::paths::AppPaths::resolve()
        .expect("resolve user paths")
        .install_bin;
    let output = Command::new(bridge)
        .args([
            "-p",
            "Reply with exactly SPARK_OK and nothing else.",
            "--output-format",
            "json",
            "--max-turns",
            "1",
            "--tools",
            "",
            "--effort",
            "max",
            "--model",
            model,
        ])
        .output()
        .expect("run installed Spark model");
    assert!(
        output.status.success(),
        "Spark failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("Claude JSON result");
    assert_eq!(result["result"], "SPARK_OK");
    assert!(result["modelUsage"][model].is_object());
}
