use std::fs;
use std::os::unix::fs::PermissionsExt;

use claude_gpt::cli::{BridgeCommand, parse_from};
use claude_gpt::compatibility::Compatibility;
use claude_gpt::paths::AppPaths;

fn write_executable(path: &std::path::Path) {
    fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
    fs::write(path, "#!/bin/sh\nexit 0\n").expect("write executable fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("make fixture executable");
}

#[test]
fn resolves_the_newest_installed_extension_without_a_pinned_version() {
    let directory = tempfile::tempdir().expect("temporary home");
    let home = directory.path().join("home");
    write_executable(&home.join(".local/bin/claude"));
    for version in ["2.1.266", "2.1.999"] {
        let extension = home.join(format!(
            ".vscode/extensions/anthropic.claude-code-{version}-darwin-arm64"
        ));
        fs::create_dir_all(&extension).expect("create extension fixture");
        fs::write(
            extension.join("package.json"),
            format!("{{\"version\":\"{version}\"}}"),
        )
        .expect("write package fixture");
        write_executable(&extension.join("resources/native-binary/claude"));
    }

    let paths = AppPaths::for_home(home.clone()).expect("discover installed runtime");

    assert_eq!(paths.claude_cli, home.join(".local/bin/claude"));
    assert_eq!(paths.codex_home, home.join(".codex"));
    assert_eq!(
        paths.vscode_extension,
        home.join(".vscode/extensions/anthropic.claude-code-2.1.999-darwin-arm64")
    );
    assert_eq!(
        paths.vscode_claude,
        home.join(
            ".vscode/extensions/anthropic.claude-code-2.1.999-darwin-arm64/resources/native-binary/claude"
        )
    );
    assert_eq!(
        paths.state_dir,
        home.join("Library/Application Support/claude-gpt")
    );
    assert_eq!(paths.install_bin, home.join(".local/bin/claude-gpt"));
}

#[test]
fn compatibility_has_no_embedded_version_manifest() {
    Compatibility::embedded().expect("runtime compatibility check initializes without pins");
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
