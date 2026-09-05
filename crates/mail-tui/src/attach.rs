//! Keyboard-only file attachment: validating a typed path, and shell-like
//! Tab completion so typing an absolute path blind isn't painful.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AttachmentEntry {
    pub path: PathBuf,
    pub filename: String,
    pub size_bytes: u64,
}

/// Expands a leading `~` to $HOME — the one shell convenience worth
/// supporting for a plain-text path field with no real shell behind it.
fn expand_tilde(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            if let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) {
                return home.join(rest.trim_start_matches('/'));
            }
        }
    }
    PathBuf::from(input)
}

/// Validates a typed path and stats the file. Returns a human-readable
/// error rather than an `anyhow`/`mail_core::Error` since this is pure
/// UI-side input validation, shown directly in the attach prompt.
pub fn validate(input: &str) -> Result<AttachmentEntry, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("enter a file path".to_string());
    }
    let path = expand_tilde(trimmed);

    let metadata = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if metadata.is_dir() {
        return Err(format!("{} is a directory", path.display()));
    }
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| trimmed.to_string());

    Ok(AttachmentEntry {
        path,
        filename,
        size_bytes: metadata.len(),
    })
}

/// Shell-style Tab completion: given what's typed so far, lists sibling
/// entries in the parent directory matching the partial last segment and
/// completes to either the single match or their longest common prefix.
/// Returns `None` when there's nothing to add (no matches, or already at
/// the longest common prefix).
pub fn complete(input: &str) -> Option<String> {
    let expanded = expand_tilde(input);
    let (dir, partial): (PathBuf, String) = if input.ends_with('/') {
        (expanded.clone(), String::new())
    } else {
        let parent = expanded.parent().map(Path::to_path_buf).unwrap_or_default();
        let partial = expanded
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        (parent, partial)
    };

    let dir_to_read = if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir };
    let entries = std::fs::read_dir(&dir_to_read).ok()?;

    let mut matches: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(&partial) {
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                Some(if is_dir { format!("{name}/") } else { name })
            } else {
                None
            }
        })
        .collect();
    matches.sort();

    if matches.is_empty() {
        return None;
    }

    let completed_suffix = if matches.len() == 1 {
        matches.remove(0)
    } else {
        longest_common_prefix(&matches)
    };
    if completed_suffix.is_empty() || completed_suffix == partial {
        return None;
    }

    let base = input.strip_suffix(&partial).unwrap_or(input);
    Some(format!("{base}{completed_suffix}"))
}

fn longest_common_prefix(strings: &[String]) -> String {
    let mut iter = strings.iter();
    let Some(first) = iter.next() else {
        return String::new();
    };
    let mut prefix_len = first.chars().count();
    for s in iter {
        prefix_len = prefix_len.min(
            first
                .chars()
                .zip(s.chars())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    first.chars().take(prefix_len).collect()
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
