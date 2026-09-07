use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config_patch::{patch_jsonc_string, restore_jsonc_string, write_atomic};
use crate::error::{BridgeError, Result};
use crate::paths::AppPaths;

const WRAPPER_KEY: &str = "claudeCode.claudeProcessWrapper";
const USAGE_COMMAND: &[u8] = include_bytes!("../commands/codex-usage.md");

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedFile {
    contents: String,
    mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallManifest {
    installed_wrapper: String,
    previous_wrapper: Option<String>,
    binary_sha256: String,
    command_sha256: String,
    previous_binary: Option<SavedFile>,
    previous_command: Option<SavedFile>,
    settings_backup: PathBuf,
}

pub fn install_from(paths: &AppPaths, source_binary: &Path) -> Result<()> {
    if paths.install_manifest.exists() {
        return repair_from(paths, source_binary);
    }
    let settings_bytes = read_or_default_settings(&paths.vscode_settings)?;
    let settings_text = String::from_utf8(settings_bytes.clone()).map_err(|error| {
        BridgeError::InvalidConfig(format!(
            "{} is not UTF-8: {error}",
            paths.vscode_settings.display()
        ))
    })?;
    let installed_wrapper = path_text(&paths.install_bin)?;
    let patch = patch_jsonc_string(&settings_text, WRAPPER_KEY, &installed_wrapper)?;
    let source_bytes = read_file(source_binary)?;
    let previous_binary = saved_file(&paths.install_bin)?;
    let command_path = usage_command_path(paths);
    let previous_command = saved_file(&command_path)?;
    let backup_path = settings_backup_path(paths)?;

    write_atomic(&backup_path, &settings_bytes, 0o600)?;
    write_atomic(&paths.install_bin, &source_bytes, 0o755)?;
    write_atomic(&command_path, USAGE_COMMAND, 0o600)?;
    write_atomic(
        &paths.vscode_settings,
        patch.text.as_bytes(),
        existing_mode(&paths.vscode_settings, 0o600),
    )?;

    let manifest = InstallManifest {
        installed_wrapper,
        previous_wrapper: patch.previous,
        binary_sha256: sha256_bytes(&source_bytes),
        command_sha256: sha256_bytes(USAGE_COMMAND),
        previous_binary,
        previous_command,
        settings_backup: backup_path,
    };
    save_manifest(paths, &manifest)
}

pub fn repair_from(paths: &AppPaths, source_binary: &Path) -> Result<()> {
    let mut manifest = load_manifest(paths)?;
    let settings_bytes = read_or_default_settings(&paths.vscode_settings)?;
    let settings = String::from_utf8(settings_bytes).map_err(|error| {
        BridgeError::InvalidConfig(format!(
            "{} is not UTF-8: {error}",
            paths.vscode_settings.display()
        ))
    })?;
    let patch = patch_jsonc_string(&settings, WRAPPER_KEY, &manifest.installed_wrapper)?;
    if patch.previous.as_deref().is_some_and(|value| {
        value != manifest.installed_wrapper && Some(value) != manifest.previous_wrapper.as_deref()
    }) {
        return Err(BridgeError::Conflict(format!(
            "{WRAPPER_KEY:?} was changed after installation"
        )));
    }
    let source_bytes = read_file(source_binary)?;
    manifest.binary_sha256 = sha256_bytes(&source_bytes);
    manifest.command_sha256 = sha256_bytes(USAGE_COMMAND);
    write_atomic(&paths.install_bin, &source_bytes, 0o755)?;
    write_atomic(&usage_command_path(paths), USAGE_COMMAND, 0o600)?;
    write_atomic(
        &paths.vscode_settings,
        patch.text.as_bytes(),
        existing_mode(&paths.vscode_settings, 0o600),
    )?;
    save_manifest(paths, &manifest)
}

pub fn uninstall_at(paths: &AppPaths) -> Result<()> {
    let manifest = load_manifest(paths)?;
    let settings_bytes = read_file(&paths.vscode_settings)?;
    let settings = String::from_utf8(settings_bytes).map_err(|error| {
        BridgeError::InvalidConfig(format!(
            "{} is not UTF-8: {error}",
            paths.vscode_settings.display()
        ))
    })?;
    let restored_settings = restore_jsonc_string(
        &settings,
        WRAPPER_KEY,
        &manifest.installed_wrapper,
        manifest.previous_wrapper.as_deref(),
    )?;
    verify_owned_file(
        &paths.install_bin,
        &manifest.binary_sha256,
        "installed binary",
    )?;
    let command_path = usage_command_path(paths);
    verify_owned_file(&command_path, &manifest.command_sha256, "usage command")?;

    write_atomic(
        &paths.vscode_settings,
        restored_settings.as_bytes(),
        existing_mode(&paths.vscode_settings, 0o600),
    )?;
    restore_file(&paths.install_bin, manifest.previous_binary)?;
    restore_file(&command_path, manifest.previous_command)?;
    std::fs::remove_file(&paths.install_manifest).map_err(|source| BridgeError::Write {
        path: paths.install_manifest.clone(),
        source,
    })
}

fn read_or_default_settings(path: &Path) -> Result<Vec<u8>> {
    if path.exists() {
        read_file(path)
    } else {
        Ok(b"{}\n".to_vec())
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|source| BridgeError::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn saved_file(path: &Path) -> Result<Option<SavedFile>> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata = std::fs::metadata(path).map_err(|source| BridgeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(SavedFile {
        contents: STANDARD.encode(read_file(path)?),
        mode: metadata.mode() & 0o777,
    }))
}

fn restore_file(path: &Path, previous: Option<SavedFile>) -> Result<()> {
    if let Some(previous) = previous {
        let bytes = STANDARD.decode(previous.contents).map_err(|error| {
            BridgeError::InvalidConfig(format!(
                "invalid saved file data for {}: {error}",
                path.display()
            ))
        })?;
        write_atomic(path, &bytes, previous.mode)
    } else if path.exists() {
        std::fs::remove_file(path).map_err(|source| BridgeError::Write {
            path: path.to_path_buf(),
            source,
        })
    } else {
        Ok(())
    }
}

fn verify_owned_file(path: &Path, expected_hash: &str, label: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let actual = sha256_bytes(&read_file(path)?);
    if actual == expected_hash {
        Ok(())
    } else {
        Err(BridgeError::Conflict(format!(
            "{label} at {} was changed after installation",
            path.display()
        )))
    }
}

fn load_manifest(paths: &AppPaths) -> Result<InstallManifest> {
    let bytes = read_file(&paths.install_manifest)?;
    serde_json::from_slice(&bytes).map_err(|source| BridgeError::ParseJson {
        path: paths.install_manifest.clone(),
        source,
    })
}

fn save_manifest(paths: &AppPaths, manifest: &InstallManifest) -> Result<()> {
    let bytes =
        serde_json::to_vec_pretty(manifest).map_err(|source| BridgeError::SerializeJson {
            path: paths.install_manifest.clone(),
            source,
        })?;
    write_atomic(&paths.install_manifest, &bytes, 0o600)
}

fn usage_command_path(paths: &AppPaths) -> PathBuf {
    paths.claude_commands.join("codex-usage.md")
}

fn settings_backup_path(paths: &AppPaths) -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| BridgeError::InvalidConfig(format!("system clock error: {error}")))?
        .as_nanos();
    Ok(paths
        .backups_dir
        .join(format!("vscode-settings-{stamp}.json")))
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        BridgeError::InvalidConfig(format!("path is not valid UTF-8: {}", path.display()))
    })
}

fn existing_mode(path: &Path, fallback: u32) -> u32 {
    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o777)
        .unwrap_or(fallback)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
