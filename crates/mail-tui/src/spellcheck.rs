//! Compose's spellcheck: drives the system `hunspell` binary in its pipe
//! (`-a`) mode, loaded with every dictionary installed on the system at
//! once, to flag misspelled words in the message body. Checking against
//! every installed dictionary together (rather than picking just one for
//! the detected system locale) means someone who writes mail in more
//! than one language gets accurate results in each of them, as long as
//! they've installed a dictionary package per language — no extra
//! configuration needed. `detect_locale` still runs at startup so the
//! "nothing installed" status message can name the language to install
//! a dictionary for.
//!
//! Shells out to the system binary rather than linking libhunspell
//! directly (no `hunspell-sys`/`pkg-config` build dependency, unlike
//! most of this project's other dependencies) or bundling dictionaries
//! (large, per-language wordlists the user's package manager already
//! knows how to install and update). Degrades silently to "off for this
//! session" whenever the binary or a dictionary isn't there — see
//! `spawn_spellchecker` in `main.rs`.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::watch;

/// One word Hunspell flagged as misspelled, with whatever corrections it
/// suggested (empty if it had none to offer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Misspelling {
    pub word: String,
    pub suggestions: Vec<String>,
}

/// Reads the POSIX locale environment in the order the C library
/// resolves it (`LC_ALL` overrides `LC_MESSAGES` overrides `LANG`) and
/// reduces it to the `language_COUNTRY` form Hunspell's dictionary
/// filenames use (e.g. `"de_DE.UTF-8"` -> `"de_DE"`). Falls back to
/// `"en_US"` when nothing usable is set.
pub fn detect_locale() -> String {
    detect_locale_from(|name| std::env::var(name).ok())
}

/// The testable core of `detect_locale` — takes a lookup function
/// instead of reading the real process environment.
fn detect_locale_from(get_env: impl Fn(&str) -> Option<String>) -> String {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(locale) = get_env(var).and_then(|v| normalize_locale(&v)) {
            return locale;
        }
    }
    "en_US".to_string()
}

/// Strips the encoding/modifier suffix from a POSIX locale string
/// (`"de_DE.UTF-8@euro"` -> `"de_DE"`) and rejects the placeholder
/// `"C"`/`"POSIX"` locale and empty strings, neither of which carries
/// any language information.
fn normalize_locale(raw: &str) -> Option<String> {
    let base = raw.split(['.', '@']).next().unwrap_or("").trim();
    if base.is_empty() || base.eq_ignore_ascii_case("C") || base.eq_ignore_ascii_case("POSIX") {
        return None;
    }
    Some(base.to_string())
}

/// Where a Linux system normally installs Hunspell/MySpell dictionaries.
pub fn default_search_dirs() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/share/hunspell"),
        PathBuf::from("/usr/share/myspell/dicts"),
        PathBuf::from("/usr/local/share/hunspell"),
    ]
}

/// Every valid `<name>.aff`/`<name>.dic` pair found across
/// `search_dirs`, as their shared base path (without extension) — what
/// Hunspell's `-d` flag wants, one entry per installed dictionary.
/// Deliberately gathers everything rather than picking just the one
/// matching the detected system locale: Hunspell can check a word
/// against several dictionaries at once, so someone who writes mail in
/// more than one language gets accurate results in all of them, with no
/// extra configuration beyond installing the relevant dictionary
/// packages. Each directory's matches are sorted for determinism, and
/// searched in the order `search_dirs` lists them; a dictionary already
/// found under an earlier directory is skipped if a later one turns out
/// to be the same file — Debian/Ubuntu-style layouts symlink an entire
/// `/usr/share/myspell/dicts` tree back onto `/usr/share/hunspell`, and
/// loading the same dictionary twice wastes memory for nothing.
pub fn find_dictionaries(search_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for dir in search_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        let mut stems: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("aff"))
            .filter_map(|e| e.path().file_stem().and_then(|s| s.to_str()).map(str::to_string))
            .filter(|stem| has_dictionary_pair(dir, stem))
            .collect();
        stems.sort();
        for stem in stems {
            let base = dir.join(stem);
            let identity = std::fs::canonicalize(base.with_extension("dic")).unwrap_or_else(|_| base.clone());
            if seen.insert(identity) {
                found.push(base);
            }
        }
    }
    found
}

fn has_dictionary_pair(dir: &Path, stem: &str) -> bool {
    dir.join(format!("{stem}.aff")).is_file() && dir.join(format!("{stem}.dic")).is_file()
}

/// Parses one line of Hunspell's `-a` pipe output. A `&` line (a "near
/// miss" with suggestions) or a `#` line (no suggestions found) marks a
/// misspelled word; every other line — `*` exact match, `+`/`-`
/// affix/compound match, or blank — means "not misspelled" and returns
/// `None`.
fn parse_response_line(line: &str) -> Option<Misspelling> {
    if let Some(rest) = line.strip_prefix("& ") {
        // "word count offset: sugg1, sugg2, ..."
        let (head, sugg_part) = rest.split_once(':')?;
        let word = head.split_whitespace().next()?.to_string();
        let suggestions = sugg_part
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        return Some(Misspelling { word, suggestions });
    }
    if let Some(rest) = line.strip_prefix("# ") {
        // "word offset"
        let word = rest.split_whitespace().next()?.to_string();
        return Some(Misspelling { word, suggestions: Vec::new() });
    }
    None
}

/// A running `hunspell -a` process, communicating over its stdin/stdout
/// pipe. One instance is spawned per app session — the system language
/// doesn't change mid-run — and reused for every check.
pub struct SpellChecker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl SpellChecker {
    /// Spawns `hunspell -a -d <dict_bases joined by comma>` — Hunspell
    /// checks a word against every listed dictionary and only flags it
    /// if none of them recognize it, so passing several at once is how
    /// multiple languages get checked together in one process — and
    /// consumes the startup banner line. Fails if the `hunspell` binary
    /// isn't installed, `dict_bases` is empty, or it won't start with
    /// these dictionaries.
    pub async fn spawn(dict_bases: &[PathBuf]) -> io::Result<Self> {
        if dict_bases.is_empty() {
            return Err(io::Error::other("no dictionaries given"));
        }
        let dict_arg = dict_bases.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(",");

        let mut child = Command::new("hunspell")
            .arg("-a")
            .arg("-i")
            .arg("utf-8")
            .arg("-d")
            .arg(dict_arg)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("hunspell spawned without a stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("hunspell spawned without a stdout pipe"))?;
        let mut stdout = BufReader::new(stdout);

        // The first line out of "-a" mode is a version banner, not a
        // response to anything we sent — discard it before use.
        let mut banner = String::new();
        stdout.read_line(&mut banner).await?;

        Ok(Self { child, stdin, stdout })
    }

    /// Checks one line of text (it must not itself contain a newline),
    /// returning every word flagged as misspelled. A leading `^` is
    /// prepended before sending, as Hunspell's pipe protocol requires so
    /// a line that happens to start with punctuation it treats as a
    /// command character (`&`, `#`, `!`, ...) is read as literal text
    /// instead.
    pub async fn check_line(&mut self, line: &str) -> io::Result<Vec<Misspelling>> {
        self.stdin.write_all(b"^").await?;
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;

        let mut results = Vec::new();
        loop {
            let mut response = String::new();
            let bytes_read = self.stdout.read_line(&mut response).await?;
            if bytes_read == 0 {
                break; // the process exited
            }
            let response = response.trim_end_matches(['\n', '\r']);
            if response.is_empty() {
                break; // a blank line ends this input line's output block
            }
            if let Some(m) = parse_response_line(response) {
                results.push(m);
            }
        }
        Ok(results)
    }
}

impl Drop for SpellChecker {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// What compose is asking to have (re)checked next. A single line is the
/// common case — most edits (typing, backspacing within a line) touch
/// exactly one — cheap enough to run on every keystroke since it's one
/// line rather than the whole message. `Full` is used instead after an
/// edit that changes the line count (Enter, or a line-joining
/// Backspace), since that shifts every later line's index and a
/// single-line patch could no longer land on the right one.
#[derive(Clone, Default)]
pub enum SpellCheckRequest {
    #[default]
    None,
    Line {
        index: usize,
        text: String,
    },
    Full(String),
}

/// A handle to the background task that owns the running `SpellChecker`
/// and feeds it text — cheap to hold onto and send requests through.
pub struct SpellCheckHandle {
    request_tx: watch::Sender<SpellCheckRequest>,
}

impl SpellCheckHandle {
    pub fn new(request_tx: watch::Sender<SpellCheckRequest>) -> Self {
        Self { request_tx }
    }

    /// Queues a recheck of just one body line. Cheap and non-blocking: a
    /// request that arrives before the previous one was even picked up
    /// simply replaces it — only the latest state is ever worth checking.
    pub fn request_line(&self, index: usize, text: String) {
        let _ = self.request_tx.send(SpellCheckRequest::Line { index, text });
    }

    /// Queues a recheck of the whole body, line by line — used when an
    /// edit changed the line count, so per-line indices from before it
    /// can no longer be trusted.
    pub fn request_full(&self, text: String) {
        let _ = self.request_tx.send(SpellCheckRequest::Full(text));
    }
}

/// Reduces a word to the form compose's rendering keys its lookup by:
/// lowercased, with leading/trailing punctuation stripped. Needed on
/// both sides of that lookup because Hunspell doesn't always report a
/// bare word — which affix rules are active depends on the *first*
/// dictionary passed to `-d` (a loaded German dictionary's abbreviation
/// handling, for instance, can make it report "speling." rather than
/// "speling" for a sentence-final word), and compose's own tokenizer
/// normalizes independently of whatever Hunspell decided to include.
pub fn normalize_word(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

/// Checks one line, returning the normalized set of every word flagged
/// as misspelled in it — the shape compose's rendering wants for a
/// case-insensitive, punctuation-insensitive lookup.
pub async fn check_one_line(checker: &mut SpellChecker, line: &str) -> io::Result<HashSet<String>> {
    let mut words = HashSet::new();
    for m in checker.check_line(line).await? {
        words.insert(normalize_word(&m.word));
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- locale detection ---------------------------------------------------

    #[test]
    fn normalize_locale_strips_encoding_and_modifier_suffixes() {
        assert_eq!(normalize_locale("de_DE.UTF-8"), Some("de_DE".to_string()));
        assert_eq!(normalize_locale("de_DE.UTF-8@euro"), Some("de_DE".to_string()));
        assert_eq!(normalize_locale("en_US"), Some("en_US".to_string()));
    }

    #[test]
    fn normalize_locale_rejects_placeholders_and_blanks() {
        assert_eq!(normalize_locale(""), None);
        assert_eq!(normalize_locale("C"), None);
        assert_eq!(normalize_locale("posix"), None);
        assert_eq!(normalize_locale("  "), None);
    }

    #[test]
    fn detect_locale_from_prefers_lc_all_over_lc_messages_over_lang() {
        let env = |name: &str| -> Option<String> {
            match name {
                "LC_ALL" => Some("de_DE.UTF-8".to_string()),
                "LC_MESSAGES" => Some("fr_FR.UTF-8".to_string()),
                "LANG" => Some("en_US.UTF-8".to_string()),
                _ => None,
            }
        };
        assert_eq!(detect_locale_from(env), "de_DE");
    }

    #[test]
    fn detect_locale_from_skips_an_unset_or_placeholder_variable_to_the_next_one() {
        let env = |name: &str| -> Option<String> {
            match name {
                "LC_ALL" => Some("C".to_string()), // placeholder — must be skipped
                "LANG" => Some("ja_JP.UTF-8".to_string()),
                _ => None,
            }
        };
        assert_eq!(detect_locale_from(env), "ja_JP");
    }

    #[test]
    fn detect_locale_from_falls_back_to_en_us_when_nothing_is_set() {
        assert_eq!(detect_locale_from(|_| None), "en_US");
    }

    // -- dictionary discovery -------------------------------------------------

    /// A scratch directory under the OS temp dir, unique per test — these
    /// tests must never touch real system dictionaries.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "mail-tui-test-spellcheck-{label}-{}-{:?}",
                std::process::id(),
                std::time::Instant::now()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn touch(&self, name: &str) {
            std::fs::write(self.0.join(name), b"").unwrap();
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn find_dictionaries_finds_every_valid_pair_in_a_directory_sorted() {
        let dir = ScratchDir::new("multi");
        dir.touch("en_US.aff");
        dir.touch("en_US.dic");
        dir.touch("de_DE.aff");
        dir.touch("de_DE.dic");

        let found = find_dictionaries(std::slice::from_ref(&dir.0));
        assert_eq!(found, vec![dir.0.join("de_DE"), dir.0.join("en_US")]);
    }

    #[test]
    fn find_dictionaries_skips_an_aff_file_with_no_matching_dic() {
        let dir = ScratchDir::new("half-pair");
        dir.touch("de_DE.aff"); // .dic missing
        dir.touch("en_US.aff");
        dir.touch("en_US.dic");

        assert_eq!(find_dictionaries(std::slice::from_ref(&dir.0)), vec![dir.0.join("en_US")]);
    }

    #[test]
    fn find_dictionaries_returns_empty_when_the_directory_has_no_dictionaries() {
        let dir = ScratchDir::new("empty");
        assert!(find_dictionaries(std::slice::from_ref(&dir.0)).is_empty());
    }

    #[test]
    fn find_dictionaries_returns_empty_for_a_directory_that_does_not_exist() {
        let parent = ScratchDir::new("missing-parent");
        let missing = parent.0.join("does-not-exist");
        assert!(find_dictionaries(std::slice::from_ref(&missing)).is_empty());
    }

    #[test]
    fn find_dictionaries_searches_every_directory_in_the_given_order() {
        let first = ScratchDir::new("dir-order-first");
        let second = ScratchDir::new("dir-order-second");
        first.touch("en_US.aff");
        first.touch("en_US.dic");
        second.touch("de_DE.aff");
        second.touch("de_DE.dic");

        let found = find_dictionaries(&[first.0.clone(), second.0.clone()]);
        assert_eq!(found, vec![first.0.join("en_US"), second.0.join("de_DE")]);
    }

    #[test]
    fn find_dictionaries_dedups_a_symlinked_copy_of_an_already_found_dictionary() {
        // Mirrors a real layout seen in the wild: a whole
        // /usr/share/myspell/dicts tree symlinked back onto
        // /usr/share/hunspell, so the same dictionary would otherwise be
        // listed — and passed to Hunspell — twice.
        let real = ScratchDir::new("dedup-real");
        real.touch("en_US.aff");
        real.touch("en_US.dic");

        let linked = ScratchDir::new("dedup-linked");
        std::os::unix::fs::symlink(real.0.join("en_US.aff"), linked.0.join("en_US.aff")).unwrap();
        std::os::unix::fs::symlink(real.0.join("en_US.dic"), linked.0.join("en_US.dic")).unwrap();

        let found = find_dictionaries(&[real.0.clone(), linked.0.clone()]);
        assert_eq!(found, vec![real.0.join("en_US")], "the symlinked duplicate must be skipped");
    }

    // -- word normalization -----------------------------------------------------

    #[test]
    fn normalize_word_lowercases_and_strips_edge_punctuation_only() {
        assert_eq!(normalize_word("Wrold."), "wrold");
        assert_eq!(normalize_word("\"Hello\""), "hello");
        assert_eq!(normalize_word("don't"), "don't", "an internal apostrophe must survive");
        assert_eq!(normalize_word("well-known"), "well-known", "an internal hyphen must survive");
    }

    // -- Hunspell "-a" pipe response parsing -----------------------------------

    #[test]
    fn parse_response_line_reads_a_near_miss_with_suggestions() {
        let m = parse_response_line("& wrold 2 6: world, wold").unwrap();
        assert_eq!(m.word, "wrold");
        assert_eq!(m.suggestions, vec!["world".to_string(), "wold".to_string()]);
    }

    #[test]
    fn parse_response_line_reads_a_miss_with_no_suggestions() {
        let m = parse_response_line("# asdfqwerty 0").unwrap();
        assert_eq!(m.word, "asdfqwerty");
        assert!(m.suggestions.is_empty());
    }

    #[test]
    fn parse_response_line_treats_correct_word_lines_as_not_misspelled() {
        assert_eq!(parse_response_line("*"), None, "'*' is an exact dictionary match");
        assert_eq!(parse_response_line("+ root"), None, "'+' is a match via affix");
        assert_eq!(parse_response_line("- compound"), None, "'-' is a match via compound analysis");
    }

    #[test]
    fn parse_response_line_ignores_a_blank_line() {
        assert_eq!(parse_response_line(""), None);
    }

    #[test]
    fn parse_response_line_handles_a_multibyte_word_correctly() {
        // Regression guard: this parses only via ASCII-anchored prefixes
        // and whitespace splitting, so it must not panic or mis-slice on
        // non-ASCII text like German umlauts.
        let m = parse_response_line("& Häuser 1 0: Häuser").unwrap();
        assert_eq!(m.word, "Häuser");
    }
}
