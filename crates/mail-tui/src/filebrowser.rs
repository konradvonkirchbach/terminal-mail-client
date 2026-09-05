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

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory tree under the OS temp dir, unique per test —
    /// these tests must never touch real user files.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "mail-tui-test-fb-{label}-{}-{:?}",
                std::process::id(),
                std::time::Instant::now()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn populated_dir(label: &str) -> ScratchDir {
        let dir = ScratchDir::new(label);
        std::fs::write(dir.0.join("banana.txt"), b"b").unwrap();
        std::fs::write(dir.0.join("Apple.txt"), b"a").unwrap();
        std::fs::write(dir.0.join(".hidden"), b"h").unwrap();
        std::fs::create_dir(dir.0.join("zdir")).unwrap();
        std::fs::create_dir(dir.0.join(".hidden_dir")).unwrap();
        dir
    }

    #[test]
    fn open_hides_dotfiles_but_shows_regular_entries() {
        let dir = populated_dir("hide-dotfiles");
        let browser = FileBrowser::open(dir.0.clone());

        let names: Vec<&str> = browser.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"banana.txt"));
        assert!(names.contains(&"Apple.txt"));
        assert!(names.contains(&"zdir"));
        assert!(!names.contains(&".hidden"));
        assert!(!names.contains(&".hidden_dir"));
    }

    #[test]
    fn open_lists_dotdot_first_then_dirs_before_files_case_insensitively_sorted() {
        let dir = populated_dir("ordering");
        let browser = FileBrowser::open(dir.0.clone());

        let names: Vec<&str> = browser.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names[0], "..", "'..' must always come first when a parent exists");
        // dirs (just "zdir") come before files, and files are sorted
        // case-insensitively ("Apple.txt" before "banana.txt").
        assert_eq!(&names[1..], &["zdir", "Apple.txt", "banana.txt"]);
    }

    #[test]
    fn open_on_a_missing_directory_records_an_error_and_still_offers_dotdot() {
        let parent = ScratchDir::new("missing-parent");
        let browser = FileBrowser::open(parent.0.join("does-not-exist"));

        assert!(browser.error.is_some());
        assert_eq!(browser.entries.len(), 1);
        assert_eq!(browser.entries[0].name, "..");
    }

    #[test]
    fn move_up_and_down_clamp_at_the_list_bounds() {
        let dir = populated_dir("move-clamp");
        let mut browser = FileBrowser::open(dir.0.clone());
        let last = browser.entries.len() - 1;

        browser.move_up();
        assert_eq!(browser.selected, 0, "move_up must not go below 0");

        for _ in 0..browser.entries.len() + 5 {
            browser.move_down();
        }
        assert_eq!(browser.selected, last, "move_down must clamp at the last entry");
    }

    #[test]
    fn activate_on_a_directory_descends_and_reloads() {
        let dir = populated_dir("descend");
        std::fs::write(dir.0.join("zdir").join("inner.txt"), b"i").unwrap();
        let mut browser = FileBrowser::open(dir.0.clone());

        let zdir_index = browser.entries.iter().position(|e| e.name == "zdir").unwrap();
        browser.selected = zdir_index;
        let result = browser.activate();

        assert!(result.is_none(), "descending into a directory yields no path");
        assert_eq!(browser.current_dir, dir.0.join("zdir"));
        assert!(browser.entries.iter().any(|e| e.name == "inner.txt"));
    }

    #[test]
    fn activate_on_dotdot_goes_back_to_the_parent_directory() {
        let dir = populated_dir("go-up");
        let mut browser = FileBrowser::open(dir.0.join("zdir"));
        assert_eq!(browser.entries[0].name, "..");

        browser.selected = 0;
        let result = browser.activate();

        assert!(result.is_none());
        assert_eq!(browser.current_dir, dir.0);
    }

    #[test]
    fn activate_on_a_file_returns_its_full_path_without_changing_directory() {
        let dir = populated_dir("pick-file");
        let mut browser = FileBrowser::open(dir.0.clone());
        let file_index = browser.entries.iter().position(|e| e.name == "banana.txt").unwrap();
        browser.selected = file_index;

        let result = browser.activate();

        assert_eq!(result, Some(dir.0.join("banana.txt")));
        assert_eq!(browser.current_dir, dir.0, "picking a file must not change the browsed directory");
    }

    #[test]
    fn save_path_is_none_without_a_filename_field() {
        let dir = populated_dir("no-filename-field");
        let browser = FileBrowser::open(dir.0.clone());
        assert!(browser.save_path().is_none());
    }

    #[test]
    fn save_path_is_none_when_the_filename_is_blank() {
        let dir = populated_dir("blank-filename");
        let mut browser = FileBrowser::open_for_save(dir.0.clone(), "report.txt".to_string());
        browser.filename.as_mut().unwrap().value = "   ".to_string();
        assert!(browser.save_path().is_none());
    }

    #[test]
    fn save_path_joins_the_current_dir_with_the_trimmed_filename() {
        let dir = populated_dir("save-path");
        let browser = FileBrowser::open_for_save(dir.0.clone(), "  report.txt  ".to_string());
        assert_eq!(browser.save_path(), Some(dir.0.join("report.txt")));
    }

    #[test]
    fn toggle_focus_only_has_an_effect_when_a_filename_field_exists() {
        let dir = populated_dir("toggle-focus");

        let mut plain = FileBrowser::open(dir.0.clone());
        plain.toggle_focus();
        assert!(!plain.filename_focused);

        let mut save = FileBrowser::open_for_save(dir.0.clone(), "x.txt".to_string());
        assert!(!save.filename_focused);
        save.toggle_focus();
        assert!(save.filename_focused);
        save.toggle_focus();
        assert!(!save.filename_focused);
    }
}
