use std::process::Command;

use serde_json::Value;

fn run_json(arguments: &[&str]) -> Value {
    let bridge = claude_gpt::paths::AppPaths::resolve()
        .expect("resolve user paths")
        .install_bin;
    let output = Command::new(bridge)
        .args(arguments)
        .output()
        .expect("run installed bridge");
    assert!(
        output.status.success(),
        "bridge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Claude JSON result")
}

#[test]
#[ignore = "uses installed Claude and Codex subscription"]
fn real_cli_text_tool_and_resume() {
    let first = run_json(&[
        "-p",
        "Use Bash exactly once to run printf TOOL_OK, then reply exactly TOOL_OK.",
        "--output-format",
        "json",
        "--max-turns",
        "2",
        "--allowedTools",
        "Bash",
        "--effort",
        "low",
    ]);
    assert_eq!(first["result"], "TOOL_OK");
    assert_eq!(first["is_error"], false);
    let session_id = first["session_id"].as_str().expect("session ID");

    let resumed = run_json(&[
        "--resume",
        session_id,
        "-p",
        "Reply exactly RESUME_OK.",
        "--output-format",
        "json",
        "--max-turns",
        "1",
        "--effort",
        "low",
    ]);
    assert_eq!(resumed["session_id"], session_id);
    assert_eq!(resumed["result"], "RESUME_OK");
}
