use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use claude_gpt::config_patch::{patch_jsonc_string, restore_jsonc_string};
use claude_gpt::install::{install_from, repair_from, uninstall_at};
use claude_gpt::paths::AppPaths;

fn fixture() -> (tempfile::TempDir, AppPaths, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let claude = home.join(".local/bin/claude");
    fs::create_dir_all(claude.parent().unwrap()).unwrap();
    fs::write(&claude, b"fixture claude").unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let extension = home.join(".vscode/extensions/anthropic.claude-code-9.9.9-darwin-arm64");
    fs::create_dir_all(extension.join("resources/native-binary")).unwrap();
    fs::write(extension.join("package.json"), "{\"version\":\"9.9.9\"}").unwrap();
    fs::write(
        extension.join("resources/native-binary/claude"),
        b"fixture claude",
    )
    .unwrap();
    let paths = AppPaths::for_home(home).unwrap();
    fs::create_dir_all(paths.vscode_settings.parent().unwrap()).unwrap();
    fs::write(
        &paths.vscode_settings,
        "{\n  // keep this comment\n  \"editor.fontSize\": 15,\n}\n",
    )
    .unwrap();
    let source = directory.path().join("claude-gpt-source");
    fs::write(&source, b"fixture binary").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    (directory, paths, source)
}

#[test]
fn losslessly_sets_replaces_and_removes_one_top_level_jsonc_string() {
    let original = "{\n  // keep\n  \"editor.fontSize\": 15,\n}\n";
    let patched = patch_jsonc_string(
        original,
        "claudeCode.claudeProcessWrapper",
        "/Users/test/.local/bin/claude-gpt",
    )
    .unwrap();

    assert!(patched.text.contains("// keep"));
    assert_eq!(patched.previous, None);
    assert_eq!(
        patched
            .text
            .matches("claudeCode.claudeProcessWrapper")
            .count(),
        1
    );

    let restored = restore_jsonc_string(
        &patched.text,
        "claudeCode.claudeProcessWrapper",
        "/Users/test/.local/bin/claude-gpt",
        None,
    )
    .unwrap();
    assert!(!restored.contains("claudeCode.claudeProcessWrapper"));
    assert!(restored.contains("// keep"));
    assert!(restored.contains("\"editor.fontSize\": 15"));
}

#[test]
fn install_is_idempotent_and_preserves_unrelated_jsonc() {
    let (_directory, paths, source) = fixture();

    install_from(&paths, &source).unwrap();
    install_from(&paths, &source).unwrap();

    let settings = fs::read_to_string(&paths.vscode_settings).unwrap();
    assert!(settings.contains("// keep this comment"));
    assert!(settings.contains("\"editor.fontSize\": 15"));
    assert_eq!(
        settings.matches("claudeCode.claudeProcessWrapper").count(),
        1
    );
    assert!(settings.contains(paths.install_bin.to_str().unwrap()));
    assert_eq!(fs::read(&paths.install_bin).unwrap(), b"fixture binary");
    assert!(paths.claude_commands.join("codex-usage.md").is_file());
    assert!(paths.install_manifest.is_file());
    assert_eq!(fs::read_dir(&paths.backups_dir).unwrap().count(), 1);
}

#[test]
fn uninstall_restores_the_prior_wrapper_and_preserves_later_unrelated_edits() {
    let (_directory, paths, source) = fixture();
    let original = "{\n  \"claudeCode.claudeProcessWrapper\": \"/prior/wrapper\",\n  \"editor.fontSize\": 15\n}\n";
    fs::write(&paths.vscode_settings, original).unwrap();
    install_from(&paths, &source).unwrap();
    let installed = fs::read_to_string(&paths.vscode_settings)
        .unwrap()
        .replace("\"editor.fontSize\": 15", "\"editor.fontSize\": 18");
    fs::write(&paths.vscode_settings, installed).unwrap();

    uninstall_at(&paths).unwrap();

    let settings = fs::read_to_string(&paths.vscode_settings).unwrap();
    assert!(settings.contains("\"claudeCode.claudeProcessWrapper\": \"/prior/wrapper\""));
    assert!(settings.contains("\"editor.fontSize\": 18"));
    assert!(!paths.install_bin.exists());
    assert!(!paths.claude_commands.join("codex-usage.md").exists());
    assert!(!paths.install_manifest.exists());
}

#[test]
fn uninstall_refuses_to_overwrite_a_post_install_wrapper_change() {
    let (_directory, paths, source) = fixture();
    install_from(&paths, &source).unwrap();
    let installed = fs::read_to_string(&paths.vscode_settings).unwrap();
    let changed = installed.replace(
        paths.install_bin.to_str().unwrap(),
        "/Users/tester/custom-wrapper",
    );
    fs::write(&paths.vscode_settings, changed).unwrap();

    let error = uninstall_at(&paths).unwrap_err();

    assert!(error.to_string().contains("conflict"));
    assert!(paths.install_bin.exists());
    assert!(paths.install_manifest.exists());
}

#[test]
fn repair_updates_the_owned_binary_hash_without_losing_the_original_rollback() {
    let (_directory, paths, source) = fixture();
    fs::create_dir_all(paths.install_bin.parent().unwrap()).unwrap();
    fs::write(&paths.install_bin, b"original user binary").unwrap();
    install_from(&paths, &source).unwrap();
    fs::write(&source, b"new bridge binary").unwrap();

    repair_from(&paths, &source).unwrap();
    uninstall_at(&paths).unwrap();

    assert_eq!(
        fs::read(&paths.install_bin).unwrap(),
        b"original user binary"
    );
}

#[test]
fn rejects_duplicate_target_keys_in_jsonc() {
    let text = "{\"x\":\"one\",\"x\":\"two\"}";
    let error = patch_jsonc_string(text, "x", "three").unwrap_err();
    assert!(error.to_string().contains("duplicate"));
}

#[test]
fn rejects_a_non_string_existing_target() {
    let text = "{\"x\": false}";
    let error = patch_jsonc_string(text, "x", "three").unwrap_err();
    assert!(error.to_string().contains("string"));
}

#[test]
fn preserves_an_inline_comment_on_the_target_property() {
    let text = "{\n  \"x\": \"one\" // keep\n}\n";
    let patched = patch_jsonc_string(text, "x", "two").unwrap();
    assert!(patched.text.contains("\"x\": \"two\" // keep"));
}
