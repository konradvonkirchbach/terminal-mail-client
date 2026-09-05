//! Validating a file chosen via the directory browser as a compose
//! attachment.

use std::path::{Path, PathBuf};

/// Total attachment size cap for one message. Conservative relative to
/// common provider limits (Gmail/Yahoo ~25MB, Outlook ~20MB) since
/// base64-encoding attachments inflates their wire size by about a third.
pub const MAX_TOTAL_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct AttachmentEntry {
    pub path: PathBuf,
    pub filename: String,
    pub size_bytes: u64,
}

/// Validates a file the browser resolved to a concrete path: it must
/// exist, not be a directory, and fit within the remaining size budget
/// alongside whatever's already attached.
pub fn validate(path: &Path, existing_total_bytes: u64) -> Result<AttachmentEntry, String> {
    let metadata = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if metadata.is_dir() {
        return Err(format!("{} is a directory", path.display()));
    }

    let size_bytes = metadata.len();
    let total = existing_total_bytes + size_bytes;
    if total > MAX_TOTAL_ATTACHMENT_BYTES {
        return Err(format!(
            "adding {} ({}) would bring attachments to {}, over the {} limit",
            path.display(),
            human_size(size_bytes),
            human_size(total),
            human_size(MAX_TOTAL_ATTACHMENT_BYTES),
        ));
    }

    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    Ok(AttachmentEntry {
        path: path.to_path_buf(),
        filename,
        size_bytes,
    })
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
