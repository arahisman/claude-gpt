use std::path::PathBuf;

use crate::error::{BridgeError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub home: PathBuf,
    pub claude_cli: PathBuf,
    pub vscode_extension: PathBuf,
    pub vscode_package_json: PathBuf,
    pub vscode_claude: PathBuf,
    pub codex: PathBuf,
    pub codex_home: PathBuf,
    pub state_dir: PathBuf,
    pub catalog_cache: PathBuf,
    pub install_bin: PathBuf,
    pub install_manifest: PathBuf,
    pub backups_dir: PathBuf,
    pub log_file: PathBuf,
    pub vscode_settings: PathBuf,
    pub claude_commands: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or(BridgeError::MissingHome)?;
        Ok(Self::for_home(home))
    }

    pub fn for_home(home: PathBuf) -> Self {
        let vscode_extension =
            home.join(".vscode/extensions/anthropic.claude-code-2.1.260-darwin-arm64");
        let state_dir = home.join("Library/Application Support/claude-gpt");
        Self {
            claude_cli: home.join(".local/share/claude/versions/2.1.258"),
            vscode_package_json: vscode_extension.join("package.json"),
            vscode_claude: vscode_extension.join("resources/native-binary/claude"),
            vscode_extension,
            codex: PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex"),
            codex_home: home.join(".codex"),
            catalog_cache: state_dir.join("models-cache.json"),
            install_bin: home.join(".local/bin/claude-gpt"),
            install_manifest: state_dir.join("install-manifest.json"),
            backups_dir: state_dir.join("backups"),
            log_file: state_dir.join("bridge.log"),
            vscode_settings: home.join("Library/Application Support/Code/User/settings.json"),
            claude_commands: home.join(".claude/commands"),
            state_dir,
            home,
        }
    }
}
