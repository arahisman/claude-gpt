use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

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
        Self::for_home(home)
    }

    pub fn for_home(home: PathBuf) -> Result<Self> {
        let vscode_extension = discover_vscode_extension(&home)?;
        let state_dir = home.join("Library/Application Support/claude-gpt");
        Ok(Self {
            claude_cli: discover_claude_cli(&home)?,
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
        })
    }
}

fn discover_claude_cli(home: &Path) -> Result<PathBuf> {
    let current = home.join(".local/bin/claude");
    if current.is_file() {
        return Ok(current);
    }

    let versions = home.join(".local/share/claude/versions");
    let candidate = newest_claude_cli(&versions)?.ok_or_else(|| {
        BridgeError::Compatibility(format!(
            "Claude Code CLI was not found at {} or under {}",
            current.display(),
            versions.display()
        ))
    })?;
    Ok(candidate)
}

fn newest_claude_cli(directory: &Path) -> Result<Option<PathBuf>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(BridgeError::Read {
                path: directory.to_path_buf(),
                source,
            });
        }
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| BridgeError::Read {
            path: directory.to_path_buf(),
            source,
        })?;
        if entry.path().is_file() {
            candidates.push(entry.path());
        }
    }
    candidates.sort_by(|left, right| compare_numeric_names(left, right));
    Ok(candidates.pop())
}

fn discover_vscode_extension(home: &Path) -> Result<PathBuf> {
    let extensions = home.join(".vscode/extensions");
    newest_directory(&extensions, |path| {
        path.join("package.json").is_file() && path.join("resources/native-binary/claude").is_file()
    })?
    .ok_or_else(|| {
        BridgeError::Compatibility(format!(
            "installed Claude Code VS Code extension was not found under {}",
            extensions.display()
        ))
    })
}

fn newest_directory<F>(directory: &Path, is_valid: F) -> Result<Option<PathBuf>>
where
    F: Fn(&Path) -> bool,
{
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(BridgeError::Read {
                path: directory.to_path_buf(),
                source,
            });
        }
    };

    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| BridgeError::Read {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("anthropic.claude-code-") || !name.ends_with("-darwin-arm64") {
            continue;
        }
        if is_valid(&path) {
            candidates.push(path);
        }
    }
    candidates.sort_by(|left, right| compare_versions(left, right));
    Ok(candidates.pop())
}

fn compare_versions(left: &Path, right: &Path) -> Ordering {
    let version = |path: &Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("anthropic.claude-code-"))
            .and_then(|name| name.strip_suffix("-darwin-arm64"))
            .map(|version| {
                version
                    .split('.')
                    .map(|part| part.parse::<u64>().unwrap_or(0))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    version(left)
        .cmp(&version(right))
        .then_with(|| left.cmp(right))
}

fn compare_numeric_names(left: &Path, right: &Path) -> Ordering {
    let version = |path: &Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|version| {
                version
                    .split('.')
                    .map(|part| part.parse::<u64>().unwrap_or(0))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    version(left)
        .cmp(&version(right))
        .then_with(|| left.cmp(right))
}
