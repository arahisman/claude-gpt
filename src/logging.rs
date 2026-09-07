use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{BridgeError, Result};

#[derive(Debug, Clone, Serialize)]
pub struct MetadataEvent {
    pub correlation_id: String,
    pub model_id: Option<String>,
    pub status: Option<u16>,
    pub latency_ms: Option<u64>,
    pub event_kind: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

pub struct MetadataLogger {
    path: PathBuf,
    max_bytes: u64,
    retained_files: usize,
}

impl MetadataLogger {
    pub fn new(path: PathBuf) -> Self {
        Self::with_limits(path, 2 * 1024 * 1024, 2)
    }

    pub fn with_limits(path: PathBuf, max_bytes: u64, retained_files: usize) -> Self {
        Self {
            path,
            max_bytes,
            retained_files,
        }
    }

    pub fn write(&self, event: &MetadataEvent) -> Result<()> {
        let mut line = serde_json::to_vec(event).map_err(|source| BridgeError::SerializeJson {
            path: self.path.clone(),
            source,
        })?;
        line.push(b'\n');
        let current_size = std::fs::metadata(&self.path)
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        if current_size.saturating_add(line.len() as u64) > self.max_bytes {
            self.rotate()?;
        }
        let parent = self.path.parent().ok_or_else(|| {
            BridgeError::InvalidConfig(format!("log path has no parent: {}", self.path.display()))
        })?;
        std::fs::create_dir_all(parent).map_err(|source| BridgeError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)
            .map_err(|source| BridgeError::Write {
                path: self.path.clone(),
                source,
            })?;
        file.write_all(&line)
            .and_then(|_| file.flush())
            .map_err(|source| BridgeError::Write {
                path: self.path.clone(),
                source,
            })
    }

    fn rotate(&self) -> Result<()> {
        if self.retained_files == 0 {
            if self.path.exists() {
                std::fs::remove_file(&self.path).map_err(|source| BridgeError::Write {
                    path: self.path.clone(),
                    source,
                })?;
            }
            return Ok(());
        }
        for index in (1..self.retained_files).rev() {
            let source = rotated_path(&self.path, index);
            if source.exists() {
                let destination = rotated_path(&self.path, index + 1);
                std::fs::rename(&source, &destination).map_err(|error| BridgeError::Write {
                    path: destination,
                    source: error,
                })?;
            }
        }
        if self.path.exists() {
            let destination = rotated_path(&self.path, 1);
            std::fs::rename(&self.path, &destination).map_err(|error| BridgeError::Write {
                path: destination,
                source: error,
            })?;
        }
        Ok(())
    }
}

pub fn redact_text(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if [
        "authorization:",
        "bearer ",
        "prompt=",
        "account_id=",
        "token=",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        "[REDACTED]".to_string()
    } else {
        text.to_string()
    }
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.display(), index))
}
