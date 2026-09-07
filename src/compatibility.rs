use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{BridgeError, Result};
use crate::paths::AppPaths;

#[derive(Debug, Clone, Deserialize)]
pub struct ComponentPin {
    pub version: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Compatibility {
    pub architecture: String,
    pub claude_cli: ComponentPin,
    pub vscode_extension: ComponentPin,
    pub vscode_claude: ComponentPin,
    pub codex: ComponentPin,
    pub codex_git_rev: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedComponent {
    pub path: PathBuf,
    pub version: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentProbe {
    pub architecture: String,
    pub claude_cli: ProbedComponent,
    pub vscode_extension: ProbedComponent,
    pub vscode_claude: ProbedComponent,
    pub codex: ProbedComponent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedEnvironment {
    pub claude_cli: ProbedComponent,
    pub vscode_extension: ProbedComponent,
    pub vscode_claude: ProbedComponent,
    pub codex: ProbedComponent,
    pub codex_git_rev: String,
}

impl Compatibility {
    pub fn embedded() -> Result<Self> {
        serde_json::from_str(include_str!("../compatibility.json")).map_err(|source| {
            BridgeError::ParseJson {
                path: PathBuf::from("embedded compatibility.json"),
                source,
            }
        })
    }

    pub fn verify(&self, paths: &AppPaths) -> Result<VerifiedEnvironment> {
        self.verify_probe(EnvironmentProbe::collect(paths)?)
    }

    pub fn verify_probe(&self, probe: EnvironmentProbe) -> Result<VerifiedEnvironment> {
        if probe.architecture != self.architecture {
            return Err(BridgeError::Compatibility(format!(
                "unsupported architecture {}; expected {}",
                probe.architecture, self.architecture
            )));
        }

        verify_component("Claude Code CLI", &self.claude_cli, &probe.claude_cli)?;
        verify_component(
            "Claude Code for VS Code",
            &self.vscode_extension,
            &probe.vscode_extension,
        )?;
        verify_component(
            "Claude Code embedded in VS Code",
            &self.vscode_claude,
            &probe.vscode_claude,
        )?;
        verify_component("Codex CLI", &self.codex, &probe.codex)?;

        Ok(VerifiedEnvironment {
            claude_cli: probe.claude_cli,
            vscode_extension: probe.vscode_extension,
            vscode_claude: probe.vscode_claude,
            codex: probe.codex,
            codex_git_rev: self.codex_git_rev.clone(),
        })
    }
}

impl EnvironmentProbe {
    pub fn collect(paths: &AppPaths) -> Result<Self> {
        Ok(Self {
            architecture: std::env::consts::ARCH.to_owned(),
            claude_cli: ProbedComponent {
                path: paths.claude_cli.clone(),
                version: command_version(&paths.claude_cli, &["--version"], "", " (Claude Code)")?,
                sha256: sha256_file(&paths.claude_cli)?,
            },
            vscode_extension: ProbedComponent {
                path: paths.vscode_package_json.clone(),
                version: package_version(&paths.vscode_package_json)?,
                sha256: sha256_file(&paths.vscode_package_json)?,
            },
            vscode_claude: ProbedComponent {
                path: paths.vscode_claude.clone(),
                version: command_version(
                    &paths.vscode_claude,
                    &["--version"],
                    "",
                    " (Claude Code)",
                )?,
                sha256: sha256_file(&paths.vscode_claude)?,
            },
            codex: ProbedComponent {
                path: paths.codex.clone(),
                version: command_version(&paths.codex, &["--version"], "codex-cli ", "")?,
                sha256: sha256_file(&paths.codex)?,
            },
        })
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|source| BridgeError::Read {
        path: path.to_owned(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|source| BridgeError::Read {
            path: path.to_owned(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn command_version(
    path: &Path,
    args: &[&str],
    trim_prefix: &str,
    trim_suffix: &str,
) -> Result<String> {
    let output = Command::new(path)
        .args(args)
        .output()
        .map_err(|source| BridgeError::Process {
            program: path.to_owned(),
            source,
        })?;
    if !output.status.success() {
        return Err(BridgeError::Compatibility(format!(
            "{} returned {}",
            path.display(),
            output.status
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .trim()
        .trim_start_matches(trim_prefix)
        .trim_end_matches(trim_suffix)
        .to_owned())
}

fn package_version(path: &Path) -> Result<String> {
    #[derive(Deserialize)]
    struct Package {
        version: String,
    }

    let bytes = std::fs::read(path).map_err(|source| BridgeError::Read {
        path: path.to_owned(),
        source,
    })?;
    serde_json::from_slice::<Package>(&bytes)
        .map(|package| package.version)
        .map_err(|source| BridgeError::ParseJson {
            path: path.to_owned(),
            source,
        })
}

fn verify_component(label: &str, pin: &ComponentPin, actual: &ProbedComponent) -> Result<()> {
    if actual.version != pin.version {
        return Err(BridgeError::Compatibility(format!(
            "{label} version {} is not supported; expected {}",
            actual.version, pin.version
        )));
    }
    if actual.sha256 != pin.sha256 {
        return Err(BridgeError::Compatibility(format!(
            "{label} hash mismatch at {}",
            actual.path.display()
        )));
    }
    Ok(())
}
