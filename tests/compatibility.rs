use std::path::PathBuf;

use claude_gpt::cli::{BridgeCommand, parse_from};
use claude_gpt::compatibility::{Compatibility, EnvironmentProbe, ProbedComponent, sha256_file};
use claude_gpt::paths::AppPaths;

fn component(path: &str, version: &str, sha256: &str) -> ProbedComponent {
    ProbedComponent {
        path: PathBuf::from(path),
        version: version.to_owned(),
        sha256: sha256.to_owned(),
    }
}

fn pinned_probe() -> EnvironmentProbe {
    EnvironmentProbe {
        architecture: "aarch64".to_owned(),
        claude_cli: component(
            "/tmp/claude",
            "2.1.258",
            "b63136194160791c27cfa7b0403060d85eb0752991625fde8c09f9acacb17c78",
        ),
        vscode_extension: component(
            "/tmp/extension/package.json",
            "2.1.263",
            "02815d4f9d69f40501b54e2302bfee072b802a93c890bb1440bceb5108c42af2",
        ),
        vscode_claude: component(
            "/tmp/extension/resources/native-binary/claude",
            "2.1.263",
            "ef5d2909c8af49f31ab6d5487e90316777bc2fac170adfe8160716caa8aaf4f9",
        ),
        codex: component(
            "/tmp/codex",
            "0.153.4",
            "c147aa90d34139599711fb568102ceefc6319ca1ac5cb6f4056ca46a1834edd9",
        ),
    }
}

#[test]
fn accepts_the_pinned_environment() {
    let verified = Compatibility::embedded()
        .expect("embedded manifest parses")
        .verify_probe(pinned_probe())
        .expect("pinned environment is accepted");

    assert_eq!(verified.claude_cli.version, "2.1.258");
    assert_eq!(verified.vscode_extension.version, "2.1.263");
    assert_eq!(verified.vscode_claude.version, "2.1.263");
    assert_eq!(verified.codex.version, "0.153.4");
}

#[test]
fn rejects_a_non_arm64_host() {
    let mut probe = pinned_probe();
    probe.architecture = "x86_64".to_owned();

    let error = Compatibility::embedded()
        .expect("embedded manifest parses")
        .verify_probe(probe)
        .expect_err("wrong architecture must be rejected");

    assert!(
        error
            .to_string()
            .contains("unsupported architecture x86_64")
    );
}

#[test]
fn rejects_a_component_hash_mismatch() {
    let mut probe = pinned_probe();
    probe.claude_cli.sha256 = "00".repeat(32);

    let error = Compatibility::embedded()
        .expect("embedded manifest parses")
        .verify_probe(probe)
        .expect_err("wrong binary hash must be rejected");

    assert!(error.to_string().contains("Claude Code CLI hash mismatch"));
}

#[test]
fn rejects_a_component_version_mismatch() {
    let mut probe = pinned_probe();
    probe.vscode_extension.version = "2.1.261".to_owned();

    let error = Compatibility::embedded()
        .expect("embedded manifest parses")
        .verify_probe(probe)
        .expect_err("wrong extension version must be rejected");

    assert!(
        error
            .to_string()
            .contains("Claude Code for VS Code version 2.1.261 is not supported")
    );
}

#[test]
fn resolves_all_user_paths_from_one_home_directory() {
    let paths = AppPaths::for_home(PathBuf::from("/Users/tester"));

    assert_eq!(
        paths.claude_cli,
        PathBuf::from("/Users/tester/.local/share/claude/versions/2.1.258")
    );
    assert_eq!(paths.codex_home, PathBuf::from("/Users/tester/.codex"));
    assert_eq!(
        paths.vscode_claude,
        PathBuf::from(
            "/Users/tester/.vscode/extensions/anthropic.claude-code-2.1.263-darwin-arm64/resources/native-binary/claude"
        )
    );
    assert_eq!(
        paths.state_dir,
        PathBuf::from("/Users/tester/Library/Application Support/claude-gpt")
    );
    assert_eq!(
        paths.install_bin,
        PathBuf::from("/Users/tester/.local/bin/claude-gpt")
    );
}

#[test]
fn forwards_unknown_arguments_to_claude_unchanged() {
    let command = parse_from(["claude-gpt", "--resume", "session-id", "--verbose"])
        .expect("wrapper arguments parse");

    assert_eq!(
        command,
        BridgeCommand::Launch(vec![
            "--resume".into(),
            "session-id".into(),
            "--verbose".into(),
        ])
    );
}

#[test]
fn recognizes_management_subcommands_without_consuming_launch_flags() {
    assert_eq!(
        parse_from(["claude-gpt", "doctor", "--live"]).expect("doctor parses"),
        BridgeCommand::Doctor { live: true }
    );
    assert_eq!(
        parse_from(["claude-gpt", "usage"]).expect("usage parses"),
        BridgeCommand::Usage
    );
}

#[test]
fn hashes_component_bytes_with_sha256() {
    let file = tempfile::NamedTempFile::new().expect("temporary component");
    std::fs::write(file.path(), b"abc").expect("write fixture");

    assert_eq!(
        sha256_file(file.path()).expect("hash fixture"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
