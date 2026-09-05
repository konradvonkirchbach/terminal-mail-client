//! A minimal keyboard-driven directory browser, shared by "attach a file"
//! (pick an existing file) and "save an attachment" (pick a directory
//! plus a filename to save as) — the two only differ in whether a
//! filename field is present.

use std::path::{Path, PathBuf};

use crate::editable::TextInput;

#[derive(Debug, Clone)]
pub struct BrowserEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

pub struct FileBrowser {
    pub current_dir: PathBuf,
    pub entries: Vec<BrowserEntry>,
    pub selected: usize,
    pub error: Option<String>,
    /// Present only when browsing to choose a save location; `None` means
    /// this is a plain "pick an existing file" browser.
    pub filename: Option<TextInput>,
    pub filename_focused: bool,
}

impl FileBrowser {
    pub fn open(start_dir: PathBuf) -> Self {
        let mut browser = Self {
            current_dir: start_dir,
            entries: Vec::new(),
            selected: 0,
            error: None,
            filename: None,
            filename_focused: false,
        };
        browser.reload();
        browser
    }

    pub fn open_for_save(start_dir: PathBuf, default_filename: String) -> Self {
        let mut browser = Self::open(start_dir);
        browser.filename = Some(TextInput::with_value(default_filename));
        browser
    }

    fn reload(&mut self) {
        self.entries.clear();
        self.selected = 0;

        if self.current_dir.parent().is_some() {
            self.entries.push(BrowserEntry {
                name: "..".to_string(),
                is_dir: true,
                size: 0,
            });
        }

        let read_dir = match std::fs::read_dir(&self.current_dir) {
            Ok(rd) => rd,
            Err(e) => {
                self.error = Some(format!("{}: {e}", self.current_dir.display()));
                return;
            }
        };

        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in read_dir.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue; // dotfiles hidden by default; ".." is added separately above
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                dirs.push(BrowserEntry { name, is_dir: true, size: 0 });
            } else {
                files.push(BrowserEntry {
                    name,
                    is_dir: false,
                    size: metadata.len(),
                });
            }
        }
        dirs.sort_by_key(|e| e.name.to_lowercase());
        files.sort_by_key(|e| e.name.to_lowercase());
        self.entries.extend(dirs);
        self.entries.extend(files);
        self.error = None;
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if !self.entries.is_empty() {
            self.selected = (self.selected + 1).min(self.entries.len() - 1);
        }
    }

    /// Activates the selected entry: descends into a directory (including
    /// `..`), or returns the full path of a selected file.
    pub fn activate(&mut self) -> Option<PathBuf> {
        let entry = self.entries.get(self.selected)?.clone();
        if entry.is_dir {
            self.current_dir = if entry.name == ".." {
                self.current_dir
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| self.current_dir.clone())
            } else {
                self.current_dir.join(&entry.name)
            };
            self.reload();
            None
        } else {
            Some(self.current_dir.join(&entry.name))
        }
    }

    /// The path a save would write to, combining the browsed directory
    /// with the (possibly edited) filename field. Only meaningful when
    /// `filename` is `Some`.
    pub fn save_path(&self) -> Option<PathBuf> {
        self.filename
            .as_ref()
            .filter(|f| !f.value.trim().is_empty())
            .map(|f| self.current_dir.join(f.value.trim()))
    }

    pub fn toggle_focus(&mut self) {
        if self.filename.is_some() {
            self.filename_focused = !self.filename_focused;
        }
    }
}
