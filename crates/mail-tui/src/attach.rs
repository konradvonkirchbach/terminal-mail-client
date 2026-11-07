//! Validating a file chosen via the directory browser as a compose
//! attachment.

use std::path::{Path, PathBuf};

/// Total attachment size cap for one message. Conservative relative to
/// common provider limits (Gmail/Yahoo ~25MB, Outlook ~20MB) since
/// base64-encoding attachments inflates their wire size by about a third.
pub const MAX_TOTAL_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

/// Where an attachment's content comes from when the message is sent.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AttachmentSource {
    /// A local file, read at send time.
    File(PathBuf),
    /// An attachment of a received message being forwarded: the
    /// `index`-th of message `uid` in folder `folder_id`.
    Message { folder_id: i64, uid: u32, index: usize },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentEntry {
    pub source: AttachmentSource,
    pub filename: String,
    pub size_bytes: u64,
}

/// Encodes a compose's attachments for a saved draft.
pub fn to_json(entries: &[AttachmentEntry]) -> String {
    serde_json::to_string(entries).unwrap_or_else(|_| "[]".to_string())
}

/// Decodes a saved draft's attachments; anything unreadable is dropped.
pub fn from_json(json: &str) -> Vec<AttachmentEntry> {
    serde_json::from_str(json).unwrap_or_default()
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
        source: AttachmentSource::File(path.to_path_buf()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_formats_bytes_without_a_decimal() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1023), "1023 B");
    }

    #[test]
    fn human_size_switches_units_at_each_1024_boundary() {
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn human_size_caps_at_gigabytes_rather_than_overflowing_the_unit_table() {
        assert_eq!(human_size(1024u64.pow(4)), "1024.0 GB");
    }

    /// A throwaway directory, deleted again on drop — these tests must
    /// never touch real user files. `.0` is its path.
    struct ScratchDir(PathBuf, #[allow(dead_code)] tempfile::TempDir);

    impl ScratchDir {
        fn new(_label: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            Self(dir.path().to_path_buf(), dir)
        }
    }

    #[test]
    fn validate_rejects_a_nonexistent_path() {
        let dir = ScratchDir::new("missing");
        let result = validate(&dir.0.join("nope.txt"), 0);
        assert!(result.is_err());
    }

    #[test]
    fn validate_rejects_a_directory() {
        let dir = ScratchDir::new("is-dir");
        let result = validate(&dir.0, 0);
        assert!(result.unwrap_err().contains("is a directory"));
    }

    #[test]
    fn validate_accepts_a_file_within_budget_and_reports_its_filename_and_size() {
        let dir = ScratchDir::new("ok-file");
        let file = dir.0.join("report.pdf");
        std::fs::write(&file, b"hello world").unwrap();

        let entry = validate(&file, 0).unwrap();

        assert_eq!(entry.filename, "report.pdf");
        assert_eq!(entry.size_bytes, 11);
        assert_eq!(entry.source, AttachmentSource::File(file));
    }

    #[test]
    fn attachments_round_trip_through_a_saved_draft() {
        let entries = vec![
            AttachmentEntry {
                source: AttachmentSource::File(PathBuf::from("/home/me/a.pdf")),
                filename: "a.pdf".into(),
                size_bytes: 3,
            },
            AttachmentEntry {
                source: AttachmentSource::Message {
                    folder_id: 2,
                    uid: 9,
                    index: 1,
                },
                filename: "b.png".into(),
                size_bytes: 4,
            },
        ];
        assert_eq!(from_json(&to_json(&entries)), entries);
        assert!(from_json("not json").is_empty());
    }

    #[test]
    fn validate_rejects_a_file_that_would_exceed_the_total_budget() {
        let dir = ScratchDir::new("too-big");
        let file = dir.0.join("big.bin");
        std::fs::write(&file, b"12345").unwrap();

        // Already at the cap minus 1 byte — a 5-byte file must be rejected.
        let existing = MAX_TOTAL_ATTACHMENT_BYTES - 1;
        let result = validate(&file, existing);

        assert!(result.unwrap_err().contains("over the"));
    }

    #[test]
    fn validate_accepts_a_file_that_exactly_fills_the_remaining_budget() {
        let dir = ScratchDir::new("exact-fit");
        let file = dir.0.join("fits.bin");
        std::fs::write(&file, b"12345").unwrap();

        let existing = MAX_TOTAL_ATTACHMENT_BYTES - 5;
        let entry = validate(&file, existing).unwrap();

        assert_eq!(entry.size_bytes, 5);
    }
}
