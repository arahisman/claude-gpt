use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::error::{BridgeError, Result};
use crate::paths::AppPaths;

#[derive(Debug, Clone, Default)]
pub struct Compatibility;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedEnvironment {
    pub claude_cli: PathBuf,
    pub vscode_package_json: PathBuf,
    pub vscode_claude: PathBuf,
    pub codex: PathBuf,
}

impl Compatibility {
    pub fn embedded() -> Result<Self> {
        Ok(Self)
    }

    pub fn verify(&self, paths: &AppPaths) -> Result<VerifiedEnvironment> {
        if std::env::consts::ARCH != "aarch64" {
            return Err(BridgeError::Compatibility(format!(
                "unsupported architecture {}; this build requires aarch64",
                std::env::consts::ARCH
            )));
        }
        verify_executable("Claude Code CLI", &paths.claude_cli)?;
        verify_file("Claude Code VS Code package", &paths.vscode_package_json)?;
        verify_executable("Claude Code embedded in VS Code", &paths.vscode_claude)?;
        verify_executable("Codex CLI", &paths.codex)?;

        Ok(VerifiedEnvironment {
            claude_cli: paths.claude_cli.clone(),
            vscode_package_json: paths.vscode_package_json.clone(),
            vscode_claude: paths.vscode_claude.clone(),
            codex: paths.codex.clone(),
        })
    }
}

fn verify_file(label: &str, path: &Path) -> Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        Err(BridgeError::Compatibility(format!(
            "{label} is missing at {}",
            path.display()
        )))
    }
}

fn verify_executable(label: &str, path: &Path) -> Result<()> {
    verify_file(label, path)?;
    let metadata = std::fs::metadata(path).map_err(|source| BridgeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.permissions().mode() & 0o111 != 0 {
        return Ok(());
    }
    Err(BridgeError::Compatibility(format!(
        "{label} is not executable at {}",
        path.display()
    )))
}
