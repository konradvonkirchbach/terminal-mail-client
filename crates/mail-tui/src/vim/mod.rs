//! A modal (vim-style) editing engine for the compose body.
//!
//! Pure logic over a `TextArea` — no rendering and no I/O — driven one
//! key at a time through `Vim::handle_key`, so the whole thing is unit
//! testable by feeding key sequences (see `tests.rs`). Hand-rolled for
//! the same reason as `editable.rs`: the ratatui editor crates pull in a
//! second, incompatible ratatui/crossterm.
//!
//! Supported: counts; `h j k l w b e W B E 0 ^ $ gg G f F t T ; , { } %`,
//! `Ctrl-d/u`, `/ ? n N * #`; `i a I A o O s S C x X r J ~ D Y p P u
//! Ctrl-r .`; operators `d c y` with any motion or text object (`iw aw iW
//! aW ip ap` and quote/bracket objects); Visual `v`/`V`; and `:q :q! :wq
//! :x :w :{n}`. Things that need more than the buffer (quitting,
//! sending) come back as an `Outcome` for the caller to act on.

mod buffer;
mod motion;
#[cfg(test)]
mod tests;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::editable::TextArea;
use motion::{Chars, FindKind, MotionKind, Pos, Range};

/// Counts are capped so a stray `99999999p` can't paste a runaway amount.
const MAX_COUNT: usize = 9_999;
/// How many undo steps are kept.
const UNDO_LIMIT: usize = 100;
/// Lines moved by `Ctrl-d`/`Ctrl-u` — the same jump as the message list's.
const HALF_PAGE: usize = crate::app::PAGE_JUMP;

/// What the command line at the bottom is collecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdKind {
    /// `:` — an ex command.
    Ex,
    /// `/` — a forward search.
    SearchForward,
    /// `?` — a backward search.
    SearchBackward,
}

impl CmdKind {
    pub fn prompt(self) -> char {
        match self {
            Self::Ex => ':',
            Self::SearchForward => '/',
            Self::SearchBackward => '?',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Insert,
    Normal,
    Visual,
    VisualLine,
    Command(CmdKind),
}

/// Something a key asked for that's beyond the editor's own buffer — the
/// compose view decides what these actually do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    None,
    /// `:q` (`force: false`) or `:q!`.
    Quit { force: bool },
    /// `:wq` / `:x`.
    Send,
    /// A one-line message for the status line (errors like "Pattern not
    /// found", or "Already at oldest change").
    Notice(String),
}

/// The unnamed register every delete, change and yank writes to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Register {
    pub text: String,
    pub linewise: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    code: KeyCode,
    ctrl: bool,
}

impl Key {
    fn char(self) -> Option<char> {
        match self.code {
            KeyCode::Char(c) if !self.ctrl => Some(c),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor: Pos,
}

impl Snapshot {
    fn of(area: &TextArea) -> Self {
        Self { lines: area.lines.clone(), cursor: Pos::new(area.cursor_row, area.cursor_col) }
    }

    fn restore(self, area: &mut TextArea) {
        area.lines = self.lines;
        area.cursor_row = self.cursor.row;
        area.cursor_col = self.cursor.col;
    }
}

#[derive(Debug, Clone)]
struct Search {
    pattern: Vec<char>,
    backward: bool,
    whole_word: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prefix {
    /// After `g` (for `gg`).
    G,
    /// After `f F t T`, waiting for the char.
    Find(FindKind),
    /// After `r`, waiting for the replacement char.
    Replace,
    /// After `i`/`a` in operator-pending or Visual mode.
    Object { inner: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordForward(bool),
    WordBackward(bool),
    WordEnd(bool),
    LineStart,
    FirstNonBlank,
    LineEnd,
    FirstLine,
    LastLine,
    Find(FindKind, char),
    RepeatFind { reverse: bool },
    ParagraphForward,
    ParagraphBackward,
    MatchBracket,
    HalfPageDown,
    HalfPageUp,
    SearchNext { reverse: bool },
}

impl Motion {
    fn is_vertical(self) -> bool {
        matches!(self, Self::Up | Self::Down | Self::HalfPageDown | Self::HalfPageUp)
    }
}

/// A command in progress: counts, an operator waiting for its motion, a
/// prefix waiting for its argument, and the keys typed so far (vim's
/// "showcmd", displayed in the status line).
#[derive(Debug, Default)]
struct Pending {
    count: Option<usize>,
    op: Option<Op>,
    op_count: Option<usize>,
    prefix: Option<Prefix>,
    keys: String,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.count.is_none() && self.op.is_none() && self.op_count.is_none() && self.prefix.is_none()
    }
}

#[derive(Debug)]
pub struct Vim {
    pub mode: Mode,
    /// What's been typed after `:`/`/`/`?` in Command mode.
    pub command_line: String,
    pending: Pending,
    register: Register,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The buffer as it was before the change in progress — committed to
    /// `undo` once the change completes (a Normal-mode command, or a whole
    /// Insert session) if anything actually changed.
    pending_undo: Option<Snapshot>,
    last_find: Option<(FindKind, char)>,
    last_search: Option<Search>,
    /// Keys of the last change, replayed by `.`.
    last_change: Vec<Key>,
    /// Keys of the command being typed, saved to `last_change` if it
    /// turns out to change the buffer.
    recording: Option<Vec<Key>>,
    replaying: bool,
    visual_anchor: Pos,
    /// The column a run of `j`/`k` aims for (vim's "curswant"): `None`
    /// until a vertical move starts (it then captures the cursor's column),
    /// reset by any other command. `Some(usize::MAX)` after `$` means
    /// "end of whatever line".
    curswant: Option<usize>,
    keep_curswant: bool,
}

impl Default for Vim {
    fn default() -> Self {
        Self::new()
    }
}

impl Vim {
    pub fn new() -> Self {
        Self {
            mode: Mode::Normal,
            command_line: String::new(),
            pending: Pending::default(),
            register: Register::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            pending_undo: None,
            last_find: None,
            last_search: None,
            last_change: Vec::new(),
            recording: None,
            replaying: false,
            visual_anchor: Pos::default(),
            curswant: None,
            keep_curswant: false,
        }
    }

    /// Enters Insert mode as if focus just moved into the editor: the
    /// whole session until Esc becomes one undo step.
    pub fn enter_insert(&mut self, area: &TextArea) {
        self.reset_transient();
        self.mode = Mode::Insert;
        if self.pending_undo.is_none() {
            self.pending_undo = Some(Snapshot::of(area));
        }
    }

    /// Enters Normal mode directly (focus moved in via Esc from a header
    /// field).
    pub fn enter_normal(&mut self, area: &mut TextArea) {
        self.commit_undo(area);
        self.reset_transient();
        self.mode = Mode::Normal;
        clamp_cursor(area, false);
    }

    /// Focus is leaving the editor: finishes whatever was in progress so
    /// an Insert session still lands in the undo history.
    pub fn leave(&mut self, area: &TextArea) {
        self.commit_undo(area);
        self.reset_transient();
        self.mode = Mode::Normal;
    }

    fn reset_transient(&mut self) {
        self.pending = Pending::default();
        self.recording = None;
        self.command_line.clear();
        self.curswant = None;
    }

    /// The keys of a partly typed command (`2d`, `ci`, ...), for display.
    pub fn pending_keys(&self) -> &str {
        &self.pending.keys
    }

    #[cfg(test)]
    pub fn register(&self) -> &Register {
        &self.register
    }

    /// The Visual selection's char range `[start, end)` on `row`, if the
    /// row is (partly) selected.
    pub fn selection_on_row(&self, area: &TextArea, row: usize) -> Option<(usize, usize)> {
        if !matches!(self.mode, Mode::Visual | Mode::VisualLine) || row >= area.lines.len() {
            return None;
        }
        let anchor = self.clamped_anchor(area);
        let cursor = Pos::new(area.cursor_row, area.cursor_col);
        let (s, e) = ordered(anchor, cursor);
        if row < s.row || row > e.row {
            return None;
        }
        let len = buffer::line_len(area, row);
        if self.mode == Mode::VisualLine {
            return Some((0, len));
        }
        let start = if row == s.row { s.col.min(len) } else { 0 };
        let end = if row == e.row { (e.col + 1).min(len) } else { len };
        Some((start, end.max(start)))
    }

    /// Feeds one key to the editor.
    pub fn handle_key(&mut self, area: &mut TextArea, code: KeyCode, modifiers: KeyModifiers) -> Outcome {
        let key = Key { code, ctrl: modifiers.contains(KeyModifiers::CONTROL) };
        if self.pending_undo.is_none() {
            self.pending_undo = Some(Snapshot::of(area));
        }
        if !self.replaying {
            match &mut self.recording {
                Some(keys) => keys.push(key),
                None if self.mode == Mode::Normal && self.pending.is_empty() => self.recording = Some(vec![key]),
                None => {}
            }
        }
        self.keep_curswant = false;

        let outcome = self.dispatch(area, key);

        clamp_cursor(area, self.mode == Mode::Insert);
        if !self.keep_curswant {
            self.curswant = None;
        }
        if !self.replaying && self.pending.is_empty() {
            match self.mode {
                Mode::Normal => {
                    if let Some(keys) = self.recording.take() {
                        if self.changed_since_snapshot(area) {
                            self.last_change = keys;
                        }
                    }
                }
                Mode::Insert => {} // keeps recording until Esc
                _ => self.recording = None,
            }
        }
        if self.mode != Mode::Insert {
            self.commit_undo(area);
        }
        outcome
    }

    fn changed_since_snapshot(&self, area: &TextArea) -> bool {
        self.pending_undo.as_ref().is_some_and(|s| s.lines != area.lines)
    }

    fn commit_undo(&mut self, area: &TextArea) {
        if let Some(snapshot) = self.pending_undo.take() {
            if snapshot.lines != area.lines {
                self.undo.push(snapshot);
                if self.undo.len() > UNDO_LIMIT {
                    self.undo.remove(0);
                }
                self.redo.clear();
            }
        }
    }

    fn dispatch(&mut self, area: &mut TextArea, key: Key) -> Outcome {
        clamp_cursor(area, self.mode == Mode::Insert);
        match self.mode {
            Mode::Insert => {
                self.insert_key(area, key);
                Outcome::None
            }
            Mode::Normal | Mode::Visual | Mode::VisualLine => self.normal_or_visual_key(area, key),
            Mode::Command(kind) => self.command_key(area, key, kind),
        }
    }

    // -- Insert mode -----------------------------------------------------

    fn insert_key(&mut self, area: &mut TextArea, key: Key) {
        match (key.code, key.ctrl) {
            (KeyCode::Esc, _) => {
                self.mode = Mode::Normal;
                area.cursor_col = area.cursor_col.saturating_sub(1);
            }
            (KeyCode::Char('w'), true) => buffer::delete_word_before(area),
            (KeyCode::Char('u'), true) => buffer::delete_to_line_start(area),
            (KeyCode::Char('h'), true) | (KeyCode::Backspace, _) => area.backspace(),
            (KeyCode::Char(c), false) => area.insert(c),
            (KeyCode::Enter, _) => area.newline(),
            (KeyCode::Delete, _) => buffer::delete_forward(area),
            (KeyCode::Left, _) => area.left(),
            (KeyCode::Right, _) => area.right(),
            (KeyCode::Up, _) => area.up(),
            (KeyCode::Down, _) => area.down(),
            (KeyCode::Home, _) => area.home(),
            (KeyCode::End, _) => area.end(),
            _ => {}
        }
    }

    // -- Normal and Visual mode ----------------------------------------------

    fn normal_or_visual_key(&mut self, area: &mut TextArea, key: Key) -> Outcome {
        if let Some(prefix) = self.pending.prefix.take() {
            self.complete_prefix(area, key, prefix);
            return Outcome::None;
        }
        if key.code == KeyCode::Esc {
            self.pending = Pending::default();
            if self.is_visual() {
                self.mode = Mode::Normal;
            }
            return Outcome::None;
        }
        if let Some(c) = key.char() {
            if c.is_ascii_digit() && (c != '0' || self.count_in_progress()) {
                self.push_count_digit(c);
                return Outcome::None;
            }
        }
        if let Some(op) = self.pending.op {
            self.operator_pending_key(area, key, op);
            return Outcome::None;
        }
        if self.is_visual() {
            self.visual_command(area, key);
            Outcome::None
        } else {
            self.normal_command(area, key)
        }
    }

    fn is_visual(&self) -> bool {
        matches!(self.mode, Mode::Visual | Mode::VisualLine)
    }

    fn count_in_progress(&self) -> bool {
        if self.pending.op.is_some() {
            self.pending.op_count.is_some()
        } else {
            self.pending.count.is_some()
        }
    }

    fn push_count_digit(&mut self, c: char) {
        let digit = c.to_digit(10).unwrap_or(0) as usize;
        let slot = if self.pending.op.is_some() { &mut self.pending.op_count } else { &mut self.pending.count };
        *slot = Some(slot.unwrap_or(0).saturating_mul(10).saturating_add(digit).min(MAX_COUNT));
        self.pending.keys.push(c);
    }

    /// The effective count (`2d3w` = 6) and whether one was typed at all.
    fn take_count(&mut self) -> (usize, bool) {
        let a = self.pending.count.take();
        let b = self.pending.op_count.take();
        let explicit = a.is_some() || b.is_some();
        (a.unwrap_or(1).saturating_mul(b.unwrap_or(1)).clamp(1, MAX_COUNT), explicit)
    }

    fn reset_pending(&mut self) {
        self.pending = Pending::default();
    }

    /// The single-key motions (prefixed ones — `gg`, `f{c}` — are parsed
    /// through `Prefix`).
    fn motion_for_key(key: Key) -> Option<Motion> {
        if key.ctrl {
            return match key.code {
                KeyCode::Char('d') => Some(Motion::HalfPageDown),
                KeyCode::Char('u') => Some(Motion::HalfPageUp),
                _ => None,
            };
        }
        Some(match key.code {
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => Motion::Left,
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Char(' ') => Motion::Right,
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Enter => Motion::Down,
            KeyCode::Char('k') | KeyCode::Up => Motion::Up,
            KeyCode::Char('w') => Motion::WordForward(false),
            KeyCode::Char('W') => Motion::WordForward(true),
            KeyCode::Char('b') => Motion::WordBackward(false),
            KeyCode::Char('B') => Motion::WordBackward(true),
            KeyCode::Char('e') => Motion::WordEnd(false),
            KeyCode::Char('E') => Motion::WordEnd(true),
            KeyCode::Char('0') | KeyCode::Home => Motion::LineStart,
            KeyCode::Char('^') => Motion::FirstNonBlank,
            KeyCode::Char('$') | KeyCode::End => Motion::LineEnd,
            KeyCode::Char('G') => Motion::LastLine,
            KeyCode::Char('}') => Motion::ParagraphForward,
            KeyCode::Char('{') => Motion::ParagraphBackward,
            KeyCode::Char('%') => Motion::MatchBracket,
            KeyCode::Char(';') => Motion::RepeatFind { reverse: false },
            KeyCode::Char(',') => Motion::RepeatFind { reverse: true },
            KeyCode::Char('n') => Motion::SearchNext { reverse: false },
            KeyCode::Char('N') => Motion::SearchNext { reverse: true },
            _ => return None,
        })
    }

    fn find_kind_for(c: char) -> Option<FindKind> {
        Some(match c {
            'f' => FindKind::Forward,
            'F' => FindKind::Backward,
            't' => FindKind::TillForward,
            'T' => FindKind::TillBackward,
            _ => return None,
        })
    }

    /// Keys shared by Normal, Visual and operator-pending modes that start
    /// a prefixed motion (`g`, `f F t T`). Returns whether `key` was one.
    fn start_motion_prefix(&mut self, key: Key) -> bool {
        let Some(c) = key.char() else { return false };
        let prefix = match c {
            'g' => Prefix::G,
            _ => match Self::find_kind_for(c) {
                Some(kind) => Prefix::Find(kind),
                None => return false,
            },
        };
        self.pending.prefix = Some(prefix);
        self.pending.keys.push(c);
        true
    }

    fn complete_prefix(&mut self, area: &mut TextArea, key: Key, prefix: Prefix) {
        match (prefix, key.char()) {
            (Prefix::G, Some('g')) => self.run_motion(area, Motion::FirstLine),
            (Prefix::Find(kind), Some(ch)) => self.run_motion(area, Motion::Find(kind, ch)),
            (Prefix::Replace, Some(ch)) => {
                let (count, _) = self.take_count();
                let at = cursor(area);
                buffer::replace_chars(area, at, count, ch);
                self.reset_pending();
            }
            (Prefix::Object { inner }, Some(obj)) => self.select_text_object(area, obj, inner),
            _ => self.reset_pending(), // Esc or anything unexpected cancels
        }
    }

    /// Runs `motion` in whatever context is pending: applies the pending
    /// operator over it, or just moves the cursor (Normal and Visual).
    fn run_motion(&mut self, area: &mut TextArea, motion: Motion) {
        let (count, explicit) = self.take_count();
        match self.pending.op {
            Some(op) => {
                let start = cursor(area);
                // `cw` changes to the end of the word, not up to the next
                // one — vim's long-standing special case.
                let motion = match motion {
                    Motion::WordForward(big) if op == Op::Change => {
                        let b = Chars::new(&area.lines);
                        if b.at(start).is_some_and(|c| !c.is_whitespace()) {
                            let first = if motion::at_word_end(&b, start, big) {
                                start
                            } else {
                                motion::word_end(&b, start, big, 1)
                            };
                            let target = motion::word_end(&b, first, big, count - 1);
                            let range = Range::chars(start, Pos::new(target.row, target.col + 1));
                            self.apply_op(area, op, range);
                            self.reset_pending();
                            return;
                        }
                        motion
                    }
                    other => other,
                };
                if let Some((target, kind)) = self.eval_motion(area, motion, count, explicit) {
                    let range = operator_range(area, start, target, kind, matches!(motion, Motion::WordForward(_)));
                    self.apply_op(area, op, range);
                }
            }
            None => {
                if let Some((target, _)) = self.eval_motion(area, motion, count, explicit) {
                    area.cursor_row = target.row;
                    area.cursor_col = target.col;
                }
            }
        }
        self.reset_pending();
    }

    fn eval_motion(&mut self, area: &TextArea, motion: Motion, count: usize, explicit: bool) -> Option<(Pos, MotionKind)> {
        let b = Chars::new(&area.lines);
        let cur = cursor(area);
        let rows = b.rows();
        // The column a run of vertical moves aims for: whatever it was
        // when the run started, so passing through a short line doesn't
        // drag the cursor left for good.
        let want = self.curswant.unwrap_or(cur.col);
        if motion.is_vertical() {
            self.curswant = Some(want);
            self.keep_curswant = true;
        }
        let vertical = |row: usize| {
            let len = b.len(row);
            let col = if want == usize::MAX { len.saturating_sub(1) } else { want.min(len) };
            (Pos::new(row, col), MotionKind::Linewise)
        };
        match motion {
            Motion::Left => (cur.col > 0).then(|| (Pos::new(cur.row, cur.col.saturating_sub(count)), MotionKind::Exclusive)),
            Motion::Right => {
                let target = (cur.col + count).min(b.len(cur.row));
                (target != cur.col).then_some((Pos::new(cur.row, target), MotionKind::Exclusive))
            }
            Motion::Up => (cur.row > 0).then(|| vertical(cur.row.saturating_sub(count))),
            Motion::Down => (cur.row + 1 < rows).then(|| vertical((cur.row + count).min(rows - 1))),
            Motion::HalfPageUp => (cur.row > 0).then(|| vertical(cur.row.saturating_sub(HALF_PAGE))),
            Motion::HalfPageDown => (cur.row + 1 < rows).then(|| vertical((cur.row + HALF_PAGE).min(rows - 1))),
            Motion::WordForward(big) => Some((motion::word_forward(&b, cur, big, count), MotionKind::Exclusive)),
            Motion::WordBackward(big) => Some((motion::word_backward(&b, cur, big, count), MotionKind::Exclusive)),
            Motion::WordEnd(big) => Some((motion::word_end(&b, cur, big, count), MotionKind::Inclusive)),
            Motion::LineStart => Some((Pos::new(cur.row, 0), MotionKind::Exclusive)),
            Motion::FirstNonBlank => Some((Pos::new(cur.row, motion::first_non_blank(&b, cur.row)), MotionKind::Exclusive)),
            Motion::LineEnd => {
                let row = (cur.row + count - 1).min(rows - 1);
                self.curswant = Some(usize::MAX);
                self.keep_curswant = true;
                Some((Pos::new(row, b.len(row)), MotionKind::Exclusive))
            }
            Motion::FirstLine | Motion::LastLine => {
                let row = if explicit {
                    (count - 1).min(rows - 1)
                } else if motion == Motion::FirstLine {
                    0
                } else {
                    rows - 1
                };
                Some((Pos::new(row, motion::first_non_blank(&b, row)), MotionKind::Linewise))
            }
            Motion::Find(kind, ch) => {
                self.last_find = Some((kind, ch));
                motion::find_char(&b, cur, kind, ch, count, false).map(|p| (p, kind.motion_kind()))
            }
            Motion::RepeatFind { reverse } => {
                let (kind, ch) = self.last_find?;
                let kind = if reverse { kind.reversed() } else { kind };
                motion::find_char(&b, cur, kind, ch, count, true).map(|p| (p, kind.motion_kind()))
            }
            Motion::ParagraphForward => Some((motion::paragraph_forward(&b, cur, count), MotionKind::Exclusive)),
            Motion::ParagraphBackward => Some((motion::paragraph_backward(&b, cur, count), MotionKind::Exclusive)),
            Motion::MatchBracket => motion::match_bracket(&b, cur).map(|p| (p, MotionKind::Inclusive)),
            Motion::SearchNext { reverse } => {
                let search = self.last_search.clone()?;
                let backward = search.backward != reverse;
                let mut p = cur;
                for _ in 0..count {
                    p = motion::search(&b, p, &search.pattern, backward, search.whole_word)?;
                }
                Some((p, MotionKind::Exclusive))
            }
        }
    }

    fn operator_pending_key(&mut self, area: &mut TextArea, key: Key, op: Op) {
        let op_char = match op {
            Op::Delete => 'd',
            Op::Change => 'c',
            Op::Yank => 'y',
        };
        match key.char() {
            Some(c) if c == op_char => {
                // `dd`, `cc`, `yy`: linewise over `count` lines.
                let (count, _) = self.take_count();
                let first = area.cursor_row;
                let last = (first + count - 1).min(area.lines.len() - 1);
                self.apply_op(area, op, Range::lines(first, last));
                self.reset_pending();
                return;
            }
            Some(c @ ('i' | 'a')) => {
                self.pending.prefix = Some(Prefix::Object { inner: c == 'i' });
                self.pending.keys.push(c);
                return;
            }
            _ => {}
        }
        if self.start_motion_prefix(key) {
            return;
        }
        match Self::motion_for_key(key) {
            Some(motion) => self.run_motion(area, motion),
            None => self.reset_pending(),
        }
    }

    fn select_text_object(&mut self, area: &mut TextArea, obj: char, inner: bool) {
        let b = Chars::new(&area.lines);
        let Some(range) = motion::text_object(&b, cursor(area), obj, inner) else {
            self.reset_pending();
            return;
        };
        if let Some(op) = self.pending.op {
            self.apply_op(area, op, range);
        } else if self.is_visual() {
            if range.linewise {
                self.mode = Mode::VisualLine;
            }
            self.visual_anchor = range.start;
            let end = if range.linewise || range.end.col == 0 { range.end } else { Pos::new(range.end.row, range.end.col - 1) };
            area.cursor_row = end.row;
            area.cursor_col = end.col.max(if end.row == range.start.row { range.start.col } else { 0 });
        }
        self.reset_pending();
    }

    /// Applies an operator over a resolved range. Deletes and changes fill
    /// the register; Change leaves the editor in Insert mode.
    fn apply_op(&mut self, area: &mut TextArea, op: Op, range: Range) {
        // An edit resets the sticky column even when its motion was `$`,
        // or `D` followed by `j` would snap to the end of the next line.
        self.keep_curswant = false;
        match op {
            Op::Yank => {
                self.register = Register { text: buffer::range_text(area, &range), linewise: range.linewise };
                let (start, _) = ordered(range.start, range.end);
                area.cursor_row = start.row;
                if !range.linewise {
                    area.cursor_col = start.col;
                }
            }
            Op::Delete => {
                let text = buffer::delete(area, &range);
                self.fill_register(text, range.linewise);
            }
            Op::Change => {
                let text = if range.linewise {
                    let (s, e) = ordered(range.start, range.end);
                    buffer::change_lines(area, s.row, e.row)
                } else {
                    buffer::delete(area, &range)
                };
                self.fill_register(text, range.linewise);
                self.mode = Mode::Insert;
            }
        }
    }

    fn fill_register(&mut self, text: String, linewise: bool) {
        // An empty charwise delete (e.g. `dw` on an empty line) shouldn't
        // clobber what's already there.
        if linewise || !text.is_empty() {
            self.register = Register { text, linewise };
        }
    }

    fn normal_command(&mut self, area: &mut TextArea, key: Key) -> Outcome {
        if key.ctrl && key.code == KeyCode::Char('r') {
            let (count, _) = self.take_count();
            self.reset_pending();
            return self.redo(area, count);
        }
        if self.start_motion_prefix(key) {
            return Outcome::None;
        }
        if let Some(motion) = Self::motion_for_key(key) {
            self.run_motion(area, motion);
            return Outcome::None;
        }
        let Some(c) = key.char() else {
            self.reset_pending();
            return Outcome::None;
        };
        let mut outcome = Outcome::None;
        match c {
            'd' | 'c' | 'y' => {
                self.pending.op = Some(match c {
                    'd' => Op::Delete,
                    'c' => Op::Change,
                    _ => Op::Yank,
                });
                self.pending.keys.push(c);
                return Outcome::None;
            }
            // The shorthands are just an operator plus a motion.
            'x' => self.op_with_motion(area, Op::Delete, Motion::Right),
            'X' => self.op_with_motion(area, Op::Delete, Motion::Left),
            'D' => self.op_with_motion(area, Op::Delete, Motion::LineEnd),
            'C' => self.op_with_motion(area, Op::Change, Motion::LineEnd),
            's' => {
                if buffer::line_len(area, area.cursor_row) == 0 {
                    self.mode = Mode::Insert;
                } else {
                    self.op_with_motion(area, Op::Change, Motion::Right);
                }
            }
            'S' | 'Y' => {
                let (count, _) = self.take_count();
                let first = area.cursor_row;
                let last = (first + count - 1).min(area.lines.len() - 1);
                let op = if c == 'S' { Op::Change } else { Op::Yank };
                self.apply_op(area, op, Range::lines(first, last));
            }
            'r' => {
                self.pending.prefix = Some(Prefix::Replace);
                self.pending.keys.push(c);
                return Outcome::None;
            }
            'J' => {
                let (count, _) = self.take_count();
                buffer::join(area, area.cursor_row, count.max(2));
            }
            '~' => {
                let (count, _) = self.take_count();
                let len = buffer::line_len(area, area.cursor_row);
                if len > 0 {
                    let start = cursor(area);
                    let end = Pos::new(start.row, (start.col + count).min(len));
                    buffer::map_chars(area, &Range::chars(start, end), buffer::toggle_case);
                    area.cursor_col = end.col;
                }
            }
            'p' | 'P' => {
                let (count, _) = self.take_count();
                let register = self.register.clone();
                if register.linewise || !register.text.is_empty() {
                    buffer::put(area, &register.text, register.linewise, c == 'p', count);
                }
            }
            'u' => {
                let (count, _) = self.take_count();
                outcome = self.undo(area, count);
            }
            '.' => {
                let (count, _) = self.take_count();
                self.repeat_last_change(area, count);
            }
            'i' => self.mode = Mode::Insert,
            'a' => {
                area.cursor_col = (area.cursor_col + 1).min(buffer::line_len(area, area.cursor_row));
                self.mode = Mode::Insert;
            }
            'I' => {
                area.cursor_col = motion::first_non_blank(&Chars::new(&area.lines), area.cursor_row);
                self.mode = Mode::Insert;
            }
            'A' => {
                area.cursor_col = buffer::line_len(area, area.cursor_row);
                self.mode = Mode::Insert;
            }
            'o' | 'O' => {
                let row = if c == 'o' { area.cursor_row + 1 } else { area.cursor_row };
                area.lines.insert(row, String::new());
                area.cursor_row = row;
                area.cursor_col = 0;
                self.mode = Mode::Insert;
            }
            'v' | 'V' => {
                self.visual_anchor = cursor(area);
                self.mode = if c == 'v' { Mode::Visual } else { Mode::VisualLine };
            }
            ':' => self.start_command(CmdKind::Ex),
            '/' => self.start_command(CmdKind::SearchForward),
            '?' => self.start_command(CmdKind::SearchBackward),
            '*' | '#' => outcome = self.search_word_under_cursor(area, c == '#'),
            _ => {}
        }
        self.reset_pending();
        outcome
    }

    fn op_with_motion(&mut self, area: &mut TextArea, op: Op, motion: Motion) {
        self.pending.op = Some(op);
        self.run_motion(area, motion);
    }

    fn start_command(&mut self, kind: CmdKind) {
        self.mode = Mode::Command(kind);
        self.command_line.clear();
    }

    fn search_word_under_cursor(&mut self, area: &mut TextArea, backward: bool) -> Outcome {
        let b = Chars::new(&area.lines);
        let at = cursor(area);
        let word = motion::text_object(&b, at, 'w', true).filter(|r| {
            b.at(r.start).is_some_and(motion::is_word_char)
        });
        let Some(word) = word else {
            return Outcome::Notice("No string under cursor".to_string());
        };
        let pattern: Vec<char> = b.line(word.start.row)[word.start.col..word.end.col].to_vec();
        self.last_search = Some(Search { pattern: pattern.clone(), backward, whole_word: true });
        // `#` starts from the word's beginning, or it would just find the
        // word the cursor is already on.
        let from = if backward { word.start } else { at };
        if let Some(p) = motion::search(&b, from, &pattern, backward, true) {
            area.cursor_row = p.row;
            area.cursor_col = p.col;
        }
        Outcome::None
    }

    fn undo(&mut self, area: &mut TextArea, count: usize) -> Outcome {
        // The snapshot taken for this key isn't a change of its own.
        self.pending_undo = None;
        for i in 0..count {
            let Some(snapshot) = self.undo.pop() else {
                return if i == 0 { Outcome::Notice("Already at oldest change".to_string()) } else { Outcome::None };
            };
            self.redo.push(Snapshot::of(area));
            snapshot.restore(area);
        }
        Outcome::None
    }

    fn redo(&mut self, area: &mut TextArea, count: usize) -> Outcome {
        self.pending_undo = None;
        for i in 0..count {
            let Some(snapshot) = self.redo.pop() else {
                return if i == 0 { Outcome::Notice("Already at newest change".to_string()) } else { Outcome::None };
            };
            self.undo.push(Snapshot::of(area));
            snapshot.restore(area);
        }
        Outcome::None
    }

    fn repeat_last_change(&mut self, area: &mut TextArea, count: usize) {
        // `.` itself must never become the last change, or the next `.`
        // would replay itself forever.
        self.recording = None;
        let keys = self.last_change.clone();
        if keys.is_empty() {
            return;
        }
        self.replaying = true;
        for _ in 0..count {
            for &key in &keys {
                self.dispatch(area, key);
            }
            if self.mode == Mode::Insert {
                self.dispatch(area, Key { code: KeyCode::Esc, ctrl: false });
            }
        }
        self.replaying = false;
        self.reset_pending();
    }

    fn visual_command(&mut self, area: &mut TextArea, key: Key) {
        if self.start_motion_prefix(key) {
            return;
        }
        if let Some(motion) = Self::motion_for_key(key) {
            self.run_motion(area, motion);
            return;
        }
        let Some(c) = key.char() else {
            self.reset_pending();
            return;
        };
        let range = self.visual_range(area);
        match c {
            'i' | 'a' => {
                self.pending.prefix = Some(Prefix::Object { inner: c == 'i' });
                self.pending.keys.push(c);
                return;
            }
            'o' => {
                let anchor = self.clamped_anchor(area);
                self.visual_anchor = cursor(area);
                area.cursor_row = anchor.row;
                area.cursor_col = anchor.col;
            }
            'v' => self.mode = if self.mode == Mode::Visual { Mode::Normal } else { Mode::Visual },
            'V' => self.mode = if self.mode == Mode::VisualLine { Mode::Normal } else { Mode::VisualLine },
            'd' | 'x' => {
                self.mode = Mode::Normal;
                self.apply_op(area, Op::Delete, range);
            }
            'c' | 's' => {
                self.mode = Mode::Normal;
                self.apply_op(area, Op::Change, range);
            }
            'y' => {
                self.mode = Mode::Normal;
                self.apply_op(area, Op::Yank, range);
            }
            '~' | 'u' | 'U' => {
                self.mode = Mode::Normal;
                let f: fn(char) -> String = match c {
                    '~' => buffer::toggle_case,
                    'u' => buffer::lower,
                    _ => buffer::upper,
                };
                buffer::map_chars(area, &range, f);
                let (start, _) = ordered(range.start, range.end);
                area.cursor_row = start.row;
                area.cursor_col = if range.linewise { 0 } else { start.col };
            }
            'J' => {
                self.mode = Mode::Normal;
                let (s, e) = ordered(range.start, range.end);
                buffer::join(area, s.row, (e.row - s.row + 1).max(2));
            }
            _ => {}
        }
        self.reset_pending();
    }

    fn clamped_anchor(&self, area: &TextArea) -> Pos {
        let row = self.visual_anchor.row.min(area.lines.len() - 1);
        Pos::new(row, self.visual_anchor.col.min(buffer::line_len(area, row)))
    }

    fn visual_range(&self, area: &TextArea) -> Range {
        let (s, e) = ordered(self.clamped_anchor(area), cursor(area));
        if self.mode == Mode::VisualLine {
            Range::lines(s.row, e.row)
        } else {
            Range::chars(s, Pos::new(e.row, (e.col + 1).min(buffer::line_len(area, e.row))))
        }
    }

    // -- Command line --------------------------------------------------------

    fn command_key(&mut self, area: &mut TextArea, key: Key, kind: CmdKind) -> Outcome {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.command_line.clear();
            }
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.command_line);
                self.mode = Mode::Normal;
                return match kind {
                    CmdKind::Ex => self.run_ex(area, line.trim()),
                    CmdKind::SearchForward | CmdKind::SearchBackward => {
                        self.run_search(area, &line, kind == CmdKind::SearchBackward)
                    }
                };
            }
            // Backspacing past the prompt abandons the command, as in vim.
            KeyCode::Backspace if self.command_line.is_empty() => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.command_line.pop();
            }
            KeyCode::Char(c) if !key.ctrl => self.command_line.push(c),
            _ => {}
        }
        Outcome::None
    }

    fn run_ex(&mut self, area: &mut TextArea, cmd: &str) -> Outcome {
        match cmd {
            "" => Outcome::None,
            "q" => Outcome::Quit { force: false },
            "q!" => Outcome::Quit { force: true },
            "wq" | "wq!" | "x" | "x!" => Outcome::Send,
            "w" => Outcome::Notice("Drafts aren't saved yet — :wq sends, :q! discards".to_string()),
            _ if cmd.chars().all(|c| c.is_ascii_digit()) => {
                let line: usize = cmd.parse().unwrap_or(usize::MAX);
                let row = line.saturating_sub(1).min(area.lines.len() - 1);
                area.cursor_row = row;
                area.cursor_col = motion::first_non_blank(&Chars::new(&area.lines), row);
                Outcome::None
            }
            _ => Outcome::Notice(format!("Not an editor command: {cmd}")),
        }
    }

    fn run_search(&mut self, area: &mut TextArea, pattern: &str, backward: bool) -> Outcome {
        let pattern: Vec<char> = if pattern.is_empty() {
            match &self.last_search {
                Some(s) => s.pattern.clone(),
                None => return Outcome::Notice("No previous search pattern".to_string()),
            }
        } else {
            pattern.chars().collect()
        };
        self.last_search = Some(Search { pattern: pattern.clone(), backward, whole_word: false });
        let b = Chars::new(&area.lines);
        match motion::search(&b, cursor(area), &pattern, backward, false) {
            Some(p) => {
                area.cursor_row = p.row;
                area.cursor_col = p.col;
                Outcome::None
            }
            None => Outcome::Notice(format!("Pattern not found: {}", pattern.iter().collect::<String>())),
        }
    }
}

fn cursor(area: &TextArea) -> Pos {
    Pos::new(area.cursor_row, area.cursor_col)
}

fn ordered(a: Pos, b: Pos) -> (Pos, Pos) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Keeps the buffer non-empty and the cursor on it: on a char in Normal
/// and Visual mode, allowed one past the end in Insert mode.
fn clamp_cursor(area: &mut TextArea, allow_past_end: bool) {
    if area.lines.is_empty() {
        area.lines.push(String::new());
    }
    area.cursor_row = area.cursor_row.min(area.lines.len() - 1);
    let len = buffer::line_len(area, area.cursor_row);
    let max = if allow_past_end { len } else { len.saturating_sub(1) };
    area.cursor_col = area.cursor_col.min(max);
}

/// The range an operator covers from `start` to a motion's `target`,
/// applying vim's adjustments for exclusive motions that end at the start
/// of a later line (`:h exclusive`): the end pulls back to the end of the
/// previous line, and — unless the motion was `w`/`W`, which vim special-
/// cases to never eat the line break — becomes linewise when the start
/// was at or before the line's first non-blank.
fn operator_range(area: &TextArea, start: Pos, target: Pos, kind: MotionKind, word_motion: bool) -> Range {
    let (s, e) = ordered(start, target);
    match kind {
        MotionKind::Linewise => Range::lines(s.row, e.row),
        MotionKind::Inclusive => Range::chars(s, Pos::new(e.row, e.col + 1)),
        MotionKind::Exclusive => {
            let b = Chars::new(&area.lines);
            let starts_its_line = e.row > s.row && b.line(e.row)[..e.col.min(b.len(e.row))].iter().all(|c| c.is_whitespace());
            if word_motion && starts_its_line {
                return Range::chars(s, Pos::new(e.row - 1, b.len(e.row - 1)));
            }
            if e.row > s.row && e.col == 0 {
                let end = Pos::new(e.row - 1, b.len(e.row - 1));
                if s.col <= motion::first_non_blank(&b, s.row) {
                    return Range::lines(s.row, end.row);
                }
                return Range::chars(s, end);
            }
            Range::chars(s, e)
        }
    }
}
