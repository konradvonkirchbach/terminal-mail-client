//! Pure cursor-motion, search and text-object logic for the vim engine.
//!
//! Everything here works on a `Chars` snapshot of the buffer (lines as
//! `Vec<char>`, so indexing is O(1) and always on char boundaries) and
//! returns positions — nothing mutates the buffer. Positions use the
//! same char-indexed `(row, col)` convention as `TextArea`; `col ==
//! line length` is the "newline slot" just past a line's last char,
//! which word motions treat as whitespace and exclusive operator ranges
//! use to mean "through the end of the line".

/// A char-indexed buffer position. Orders by row, then column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

impl Pos {
    pub fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// How an operator treats the span between the cursor and a motion's
/// target, mirroring vim's own classification of each motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionKind {
    /// The target char itself isn't included (`w`, `b`, `0`, `F`, ...).
    Exclusive,
    /// The target char is included (`e`, `f`, `t`, `%`).
    Inclusive,
    /// Whole lines, whatever the columns (`j`, `k`, `gg`, `G`).
    Linewise,
}

/// Which of `f F t T` a character search is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindKind {
    /// `f`: onto the next occurrence.
    Forward,
    /// `F`: onto the previous occurrence.
    Backward,
    /// `t`: just before the next occurrence.
    TillForward,
    /// `T`: just after the previous occurrence.
    TillBackward,
}

impl FindKind {
    /// The opposite direction, for `,` (repeat the last `f/t` backwards).
    pub fn reversed(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
            Self::TillForward => Self::TillBackward,
            Self::TillBackward => Self::TillForward,
        }
    }

    pub fn motion_kind(self) -> MotionKind {
        match self {
            Self::Forward | Self::TillForward => MotionKind::Inclusive,
            Self::Backward | Self::TillBackward => MotionKind::Exclusive,
        }
    }
}

/// A span of the buffer an operator acts on. Charwise ranges are
/// `[start, end)` — `end` may be a newline slot, and a range spanning
/// rows removes the newlines in between. Linewise ranges cover whole rows
/// `start.row..=end.row`, columns ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: Pos,
    pub end: Pos,
    pub linewise: bool,
}

impl Range {
    pub fn chars(start: Pos, end: Pos) -> Self {
        Self {
            start,
            end,
            linewise: false,
        }
    }

    pub fn lines(first: usize, last: usize) -> Self {
        Self {
            start: Pos::new(first, 0),
            end: Pos::new(last, 0),
            linewise: true,
        }
    }
}

/// The buffer as rows of chars — built once per key so every motion can
/// index characters directly instead of re-walking UTF-8 each time.
pub struct Chars(Vec<Vec<char>>);

impl Chars {
    pub fn new(lines: &[String]) -> Self {
        let mut rows: Vec<Vec<char>> = lines.iter().map(|l| l.chars().collect()).collect();
        if rows.is_empty() {
            rows.push(Vec::new());
        }
        Self(rows)
    }

    pub fn rows(&self) -> usize {
        self.0.len()
    }

    pub fn len(&self, row: usize) -> usize {
        self.0.get(row).map_or(0, Vec::len)
    }

    pub fn line(&self, row: usize) -> &[char] {
        &self.0[row]
    }

    pub fn at(&self, p: Pos) -> Option<char> {
        self.0.get(p.row)?.get(p.col).copied()
    }

    /// The very end of the buffer: the last row's newline slot.
    pub fn end(&self) -> Pos {
        let row = self.rows() - 1;
        Pos::new(row, self.len(row))
    }

    /// One position forward, stepping from a row's newline slot to the
    /// start of the next row. `None` at the end of the buffer.
    fn advance(&self, p: Pos) -> Option<Pos> {
        if p.col < self.len(p.row) {
            Some(Pos::new(p.row, p.col + 1))
        } else if p.row + 1 < self.rows() {
            Some(Pos::new(p.row + 1, 0))
        } else {
            None
        }
    }

    /// One position back, stepping from a row's start to the previous
    /// row's newline slot. `None` at the start of the buffer.
    fn retreat(&self, p: Pos) -> Option<Pos> {
        if p.col > 0 {
            Some(Pos::new(p.row, p.col - 1))
        } else if p.row > 0 {
            Some(Pos::new(p.row - 1, self.len(p.row - 1)))
        } else {
            None
        }
    }
}

pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The character classes vim's word motions split text on. A WORD
/// (`big`) is any run of non-blanks, so punctuation joins the word class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cls {
    Space,
    /// An empty line — its own class so `w`/`b` stop on it like vim does.
    EmptyLine,
    Word,
    Punct,
}

fn cls(b: &Chars, p: Pos, big: bool) -> Cls {
    if b.len(p.row) == 0 {
        return Cls::EmptyLine;
    }
    match b.at(p) {
        None => Cls::Space, // newline slot
        Some(c) if c.is_whitespace() => Cls::Space,
        Some(_) if big => Cls::Word,
        Some(c) if is_word_char(c) => Cls::Word,
        Some(_) => Cls::Punct,
    }
}

/// Index of the first non-blank char in `row`, or the row's length if it
/// has none.
pub fn first_non_blank(b: &Chars, row: usize) -> usize {
    b.line(row)
        .iter()
        .position(|c| !c.is_whitespace())
        .unwrap_or_else(|| b.len(row))
}

/// `w` / `W`: start of the `count`-th next word. Empty lines count as
/// words. Running off the end lands on the buffer's end.
pub fn word_forward(b: &Chars, mut p: Pos, big: bool, count: usize) -> Pos {
    for _ in 0..count {
        let start = cls(b, p, big);
        // Step off the current word (or empty line)...
        if matches!(start, Cls::Word | Cls::Punct | Cls::EmptyLine) {
            loop {
                match b.advance(p) {
                    None => return b.end(),
                    Some(n) => {
                        p = n;
                        if start == Cls::EmptyLine || cls(b, n, big) != start {
                            break;
                        }
                    }
                }
            }
        }
        // ...then over any whitespace, stopping at a word or empty line.
        while cls(b, p, big) == Cls::Space {
            match b.advance(p) {
                None => return b.end(),
                Some(n) => p = n,
            }
        }
    }
    p
}

/// `b` / `B`: start of the `count`-th previous word.
pub fn word_backward(b: &Chars, mut p: Pos, big: bool, count: usize) -> Pos {
    for _ in 0..count {
        let Some(n) = b.retreat(p) else { return p };
        p = n;
        while cls(b, p, big) == Cls::Space {
            match b.retreat(p) {
                None => return p,
                Some(n) => p = n,
            }
        }
        let c = cls(b, p, big);
        if c == Cls::EmptyLine {
            continue;
        }
        while let Some(n) = b.retreat(p) {
            if n.row != p.row || cls(b, n, big) != c {
                break;
            }
            p = n;
        }
    }
    p
}

/// `e` / `E`: end of the `count`-th word from here (skipping the current
/// char first, so repeated `e` walks word to word). Empty lines are
/// skipped rather than stopped on, as in vim.
pub fn word_end(b: &Chars, mut p: Pos, big: bool, count: usize) -> Pos {
    for _ in 0..count {
        let Some(n) = b.advance(p) else { return p };
        p = n;
        while matches!(cls(b, p, big), Cls::Space | Cls::EmptyLine) {
            match b.advance(p) {
                None => return p,
                Some(n) => p = n,
            }
        }
        let c = cls(b, p, big);
        while let Some(n) = b.advance(p) {
            if cls(b, n, big) != c {
                break;
            }
            p = n;
        }
    }
    p
}

/// Whether `p` sits on the last char of a word (the next char on the
/// line is a different class, or there isn't one).
pub fn at_word_end(b: &Chars, p: Pos, big: bool) -> bool {
    let here = cls(b, p, big);
    if !matches!(here, Cls::Word | Cls::Punct) {
        return false;
    }
    let next = Pos::new(p.row, p.col + 1);
    b.at(next).is_none() || cls(b, next, big) != here
}

/// `f F t T {ch}`: the `count`-th occurrence of `ch` on the current line.
/// `repeat` is set for `;`/`,`, which (like modern vim) skip an adjacent
/// match for `t`/`T` so repeating them actually moves.
pub fn find_char(b: &Chars, p: Pos, kind: FindKind, ch: char, count: usize, repeat: bool) -> Option<Pos> {
    let line = b.line(p.row);
    let skip = usize::from(repeat && matches!(kind, FindKind::TillForward | FindKind::TillBackward));
    let mut seen = 0;
    match kind {
        FindKind::Forward | FindKind::TillForward => {
            for (i, &c) in line.iter().enumerate().skip(p.col + 1 + skip) {
                if c == ch {
                    seen += 1;
                    if seen == count {
                        let col = if kind == FindKind::Forward { i } else { i - 1 };
                        return Some(Pos::new(p.row, col));
                    }
                }
            }
        }
        FindKind::Backward | FindKind::TillBackward => {
            let upper = p.col.saturating_sub(skip);
            for (i, &c) in line.iter().enumerate().take(upper).rev() {
                if c == ch {
                    seen += 1;
                    if seen == count {
                        let col = if kind == FindKind::Backward { i } else { i + 1 };
                        return Some(Pos::new(p.row, col));
                    }
                }
            }
        }
    }
    None
}

/// `}`: the next empty line after the current paragraph, `count` times;
/// the buffer's end if there isn't one.
pub fn paragraph_forward(b: &Chars, p: Pos, count: usize) -> Pos {
    let rows = b.rows();
    let mut r = p.row;
    for _ in 0..count {
        while r < rows && b.len(r) == 0 {
            r += 1;
        }
        while r < rows && b.len(r) != 0 {
            r += 1;
        }
        if r >= rows {
            return b.end();
        }
    }
    Pos::new(r, 0)
}

/// `{`: the previous empty line before the current paragraph, `count`
/// times; the buffer's start if there isn't one.
pub fn paragraph_backward(b: &Chars, p: Pos, count: usize) -> Pos {
    let mut r = p.row;
    for _ in 0..count {
        while r > 0 && b.len(r) == 0 {
            r -= 1;
        }
        while r > 0 && b.len(r) != 0 {
            r -= 1;
        }
        if r == 0 {
            break;
        }
    }
    Pos::new(r, 0)
}

fn bracket_pair(c: char) -> Option<(char, char, bool)> {
    Some(match c {
        '(' => ('(', ')', true),
        ')' => ('(', ')', false),
        '[' => ('[', ']', true),
        ']' => ('[', ']', false),
        '{' => ('{', '}', true),
        '}' => ('{', '}', false),
        _ => return None,
    })
}

/// `%`: the bracket matching the first `()[]{}` at or after the cursor on
/// its line, searching across lines and respecting nesting.
pub fn match_bracket(b: &Chars, p: Pos) -> Option<Pos> {
    let line = b.line(p.row);
    let (col, (open, close, is_open)) =
        (p.col..line.len()).find_map(|i| bracket_pair(line[i]).map(|pair| (i, pair)))?;
    let start = Pos::new(p.row, col);
    if is_open {
        scan_for_close(b, start, open, close)
    } else {
        scan_for_open(b, start, open, close, true)
    }
}

/// From an opening bracket at `from`, the position of its match.
fn scan_for_close(b: &Chars, from: Pos, open: char, close: char) -> Option<Pos> {
    let mut depth = 0usize;
    let mut p = from;
    while let Some(n) = b.advance(p) {
        p = n;
        match b.at(p) {
            Some(c) if c == open => depth += 1,
            Some(c) if c == close => {
                if depth == 0 {
                    return Some(p);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

/// The unmatched opening bracket enclosing `from`, scanning backwards. A
/// closing bracket right at `from` is treated as the end of the block
/// being searched for (`skip_close_at_start`), not as a nested one.
fn scan_for_open(b: &Chars, from: Pos, open: char, close: char, skip_close_at_start: bool) -> Option<Pos> {
    let mut depth = 0usize;
    let mut p = from;
    let mut first = true;
    loop {
        match b.at(p) {
            Some(c) if c == close && !(first && skip_close_at_start) => depth += 1,
            Some(c) if c == open && !(first && skip_close_at_start) => {
                if depth == 0 {
                    return Some(p);
                }
                depth -= 1;
            }
            _ => {}
        }
        first = false;
        p = b.retreat(p)?;
    }
}

/// Every match of `pattern` in the buffer, in document order. Smartcase:
/// case-insensitive unless the pattern contains an uppercase letter.
/// `whole_word` requires the match to be bounded by non-word chars (`*`,
/// `#`).
fn all_matches(b: &Chars, pattern: &[char], whole_word: bool) -> Vec<Pos> {
    if pattern.is_empty() {
        return Vec::new();
    }
    let ignore_case = !pattern.iter().any(|c| c.is_uppercase());
    let same = |a: char, c: char| {
        if ignore_case {
            a.to_lowercase().eq(c.to_lowercase())
        } else {
            a == c
        }
    };
    let mut out = Vec::new();
    for row in 0..b.rows() {
        let line = b.line(row);
        if line.len() < pattern.len() {
            continue;
        }
        for col in 0..=line.len() - pattern.len() {
            let hit = pattern.iter().enumerate().all(|(i, &c)| same(line[col + i], c));
            if !hit {
                continue;
            }
            if whole_word {
                let before_ok = col == 0 || !is_word_char(line[col - 1]);
                let end = col + pattern.len();
                let after_ok = end >= line.len() || !is_word_char(line[end]);
                if !(before_ok && after_ok) {
                    continue;
                }
            }
            out.push(Pos::new(row, col));
        }
    }
    out
}

/// The next match of `pattern` after `from` (before it, when `backward`),
/// wrapping around the buffer.
pub fn search(b: &Chars, from: Pos, pattern: &[char], backward: bool, whole_word: bool) -> Option<Pos> {
    let matches = all_matches(b, pattern, whole_word);
    if backward {
        matches
            .iter()
            .rev()
            .find(|&&m| m < from)
            .or_else(|| matches.last())
            .copied()
    } else {
        matches.iter().find(|&&m| m > from).or_else(|| matches.first()).copied()
    }
}

/// The text object named by `obj` around `p` (`iw aw iW aW ip ap i" a"
/// i' a' i( a( ib ab i[ a[ i{ a{ iB aB i< a<`), or `None` when there's
/// nothing to select (e.g. no enclosing brackets).
pub fn text_object(b: &Chars, p: Pos, obj: char, inner: bool) -> Option<Range> {
    match obj {
        'w' => word_object(b, p, false, inner),
        'W' => word_object(b, p, true, inner),
        'p' => Some(paragraph_object(b, p, inner)),
        '"' | '\'' | '`' => quote_object(b, p, obj, inner),
        '(' | ')' | 'b' => bracket_object(b, p, '(', ')', inner),
        '[' | ']' => bracket_object(b, p, '[', ']', inner),
        '{' | '}' | 'B' => bracket_object(b, p, '{', '}', inner),
        '<' | '>' => bracket_object(b, p, '<', '>', inner),
        _ => None,
    }
}

fn word_object(b: &Chars, p: Pos, big: bool, inner: bool) -> Option<Range> {
    let line = b.line(p.row);
    if line.is_empty() {
        return None;
    }
    let class_of = |c: char| -> u8 {
        if c.is_whitespace() {
            0
        } else if big || is_word_char(c) {
            1
        } else {
            2
        }
    };
    let col = p.col.min(line.len() - 1);
    let here = class_of(line[col]);
    let mut s = col;
    while s > 0 && class_of(line[s - 1]) == here {
        s -= 1;
    }
    let mut e = col + 1;
    while e < line.len() && class_of(line[e]) == here {
        e += 1;
    }
    let range = |s: usize, e: usize| Some(Range::chars(Pos::new(p.row, s), Pos::new(p.row, e)));
    if inner {
        return range(s, e);
    }
    if here == 0 {
        // `aw` on whitespace: the whitespace plus the word after it.
        if e >= line.len() {
            return range(s, e);
        }
        let next = class_of(line[e]);
        let mut e2 = e;
        while e2 < line.len() && class_of(line[e2]) == next {
            e2 += 1;
        }
        return range(s, e2);
    }
    // `aw` on a word: the word plus trailing whitespace, or leading
    // whitespace when there's no trailing.
    let mut e2 = e;
    while e2 < line.len() && line[e2].is_whitespace() {
        e2 += 1;
    }
    if e2 > e {
        return range(s, e2);
    }
    let mut s2 = s;
    while s2 > 0 && line[s2 - 1].is_whitespace() {
        s2 -= 1;
    }
    range(s2, e)
}

fn paragraph_object(b: &Chars, p: Pos, inner: bool) -> Range {
    let empty = |r: usize| b.len(r) == 0;
    let kind = empty(p.row);
    let mut s = p.row;
    while s > 0 && empty(s - 1) == kind {
        s -= 1;
    }
    let mut e = p.row;
    while e + 1 < b.rows() && empty(e + 1) == kind {
        e += 1;
    }
    if inner {
        return Range::lines(s, e);
    }
    // `ap`: also take the following run of the opposite kind (the blank
    // lines after a paragraph, or the paragraph after blank lines)...
    let mut e2 = e;
    while e2 + 1 < b.rows() && empty(e2 + 1) != kind {
        e2 += 1;
    }
    if e2 > e {
        return Range::lines(s, e2);
    }
    // ...or, with nothing after it, the blank lines before a paragraph.
    let mut s2 = s;
    if !kind {
        while s2 > 0 && empty(s2 - 1) {
            s2 -= 1;
        }
    }
    Range::lines(s2, e)
}

fn quote_object(b: &Chars, p: Pos, q: char, inner: bool) -> Option<Range> {
    let line = b.line(p.row);
    let quotes: Vec<usize> = line
        .iter()
        .enumerate()
        .filter(|(_, &c)| c == q)
        .map(|(i, _)| i)
        .collect();
    let pairs: Vec<(usize, usize)> = quotes.chunks_exact(2).map(|w| (w[0], w[1])).collect();
    let (open, close) = pairs
        .iter()
        .find(|&&(o, c)| o <= p.col && p.col <= c)
        .or_else(|| pairs.iter().find(|&&(o, _)| o > p.col))
        .copied()?;
    if inner {
        return Some(Range::chars(Pos::new(p.row, open + 1), Pos::new(p.row, close)));
    }
    let mut end = close + 1;
    while end < line.len() && line[end].is_whitespace() {
        end += 1;
    }
    let mut start = open;
    if end == close + 1 {
        while start > 0 && line[start - 1].is_whitespace() {
            start -= 1;
        }
    }
    Some(Range::chars(Pos::new(p.row, start), Pos::new(p.row, end)))
}

fn bracket_object(b: &Chars, p: Pos, open: char, close: char, inner: bool) -> Option<Range> {
    // On a closing bracket, that bracket ends the block being selected
    // rather than opening a nested level — decided upfront, or `((b))`
    // with the cursor on the inner `)` would select the outer pair.
    let on_close = b.at(p) == Some(close);
    let open_pos = scan_for_open(b, p, open, close, on_close)?;
    let close_pos = scan_for_close(b, open_pos, open, close)?;
    if inner {
        let start = b.advance(open_pos)?;
        Some(Range::chars(start, close_pos))
    } else {
        Some(Range::chars(open_pos, Pos::new(close_pos.row, close_pos.col + 1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(text: &str) -> Chars {
        let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        Chars::new(&lines)
    }

    fn p(row: usize, col: usize) -> Pos {
        Pos::new(row, col)
    }

    // -- word motions ------------------------------------------------------

    #[test]
    fn word_forward_stops_at_word_and_punctuation_boundaries() {
        let b = buf("foo.bar baz");
        assert_eq!(word_forward(&b, p(0, 0), false, 1), p(0, 3), "'.' starts a new word");
        assert_eq!(word_forward(&b, p(0, 3), false, 1), p(0, 4));
        assert_eq!(word_forward(&b, p(0, 4), false, 1), p(0, 8));
    }

    #[test]
    fn big_word_forward_treats_punctuation_as_part_of_the_word() {
        let b = buf("foo.bar baz");
        assert_eq!(word_forward(&b, p(0, 0), true, 1), p(0, 8));
    }

    #[test]
    fn word_forward_crosses_lines_and_stops_on_empty_lines() {
        let b = buf("one\n\ntwo");
        assert_eq!(
            word_forward(&b, p(0, 0), false, 1),
            p(1, 0),
            "an empty line counts as a word"
        );
        assert_eq!(word_forward(&b, p(1, 0), false, 1), p(2, 0));
    }

    #[test]
    fn word_forward_honours_the_count_and_stops_at_the_buffer_end() {
        let b = buf("a b c");
        assert_eq!(word_forward(&b, p(0, 0), false, 2), p(0, 4));
        assert_eq!(
            word_forward(&b, p(0, 0), false, 9),
            p(0, 5),
            "runs off the end onto the newline slot"
        );
    }

    #[test]
    fn word_backward_goes_to_the_start_of_the_previous_word() {
        let b = buf("foo bar.baz");
        assert_eq!(word_backward(&b, p(0, 10), false, 1), p(0, 8));
        assert_eq!(word_backward(&b, p(0, 8), false, 1), p(0, 7));
        assert_eq!(word_backward(&b, p(0, 7), false, 1), p(0, 4));
        assert_eq!(word_backward(&b, p(0, 4), false, 2), p(0, 0));
        assert_eq!(word_backward(&b, p(0, 0), false, 1), p(0, 0), "no-op at buffer start");
    }

    #[test]
    fn word_backward_crosses_lines_and_stops_on_empty_lines() {
        let b = buf("one\n\ntwo");
        assert_eq!(word_backward(&b, p(2, 0), false, 1), p(1, 0));
        assert_eq!(word_backward(&b, p(1, 0), false, 1), p(0, 0));
    }

    #[test]
    fn word_end_moves_to_the_last_char_of_the_next_word_end() {
        let b = buf("foo bar\nbaz");
        assert_eq!(word_end(&b, p(0, 0), false, 1), p(0, 2));
        assert_eq!(
            word_end(&b, p(0, 2), false, 1),
            p(0, 6),
            "from a word's end, jumps to the next one's"
        );
        assert_eq!(word_end(&b, p(0, 6), false, 1), p(1, 2), "crosses lines");
    }

    #[test]
    fn word_motions_handle_multibyte_text() {
        let b = buf("grüße aus köln");
        assert_eq!(word_forward(&b, p(0, 0), false, 1), p(0, 6));
        assert_eq!(word_end(&b, p(0, 0), false, 1), p(0, 4));
        assert_eq!(word_backward(&b, p(0, 13), false, 1), p(0, 10));
    }

    #[test]
    fn at_word_end_detects_the_last_char_of_a_word() {
        let b = buf("foo bar");
        assert!(at_word_end(&b, p(0, 2), false));
        assert!(!at_word_end(&b, p(0, 1), false));
        assert!(!at_word_end(&b, p(0, 3), false), "whitespace is never a word end");
        assert!(at_word_end(&b, p(0, 6), false), "end of line counts");
    }

    // -- line and character motions ------------------------------------------

    #[test]
    fn first_non_blank_skips_leading_whitespace() {
        let b = buf("   hi\n   ");
        assert_eq!(first_non_blank(&b, 0), 3);
        assert_eq!(first_non_blank(&b, 1), 3, "an all-blank line reports its length");
    }

    #[test]
    fn find_char_supports_all_four_kinds_and_counts() {
        let b = buf("a-b-c-d");
        assert_eq!(find_char(&b, p(0, 0), FindKind::Forward, '-', 2, false), Some(p(0, 3)));
        assert_eq!(
            find_char(&b, p(0, 0), FindKind::TillForward, 'c', 1, false),
            Some(p(0, 3))
        );
        assert_eq!(find_char(&b, p(0, 6), FindKind::Backward, '-', 1, false), Some(p(0, 5)));
        assert_eq!(
            find_char(&b, p(0, 6), FindKind::TillBackward, 'b', 1, false),
            Some(p(0, 3))
        );
        assert_eq!(find_char(&b, p(0, 0), FindKind::Forward, 'z', 1, false), None);
    }

    #[test]
    fn repeated_till_skips_an_adjacent_match_so_it_keeps_moving() {
        let b = buf("a,b,c");
        // Cursor right before the first ',' (as after `t,`): a plain `t,`
        // wouldn't move, but a repeat (`;`) goes on to the next one.
        assert_eq!(
            find_char(&b, p(0, 0), FindKind::TillForward, ',', 1, true),
            Some(p(0, 2))
        );
    }

    #[test]
    fn find_kind_reversal_and_inclusivity() {
        assert_eq!(FindKind::Forward.reversed(), FindKind::Backward);
        assert_eq!(FindKind::TillBackward.reversed(), FindKind::TillForward);
        assert_eq!(FindKind::Forward.motion_kind(), MotionKind::Inclusive);
        assert_eq!(FindKind::Backward.motion_kind(), MotionKind::Exclusive);
    }

    // -- paragraphs and brackets ---------------------------------------------

    #[test]
    fn paragraph_motions_jump_between_blank_lines() {
        let b = buf("a\nb\n\nc\n\nd");
        assert_eq!(paragraph_forward(&b, p(0, 0), 1), p(2, 0));
        assert_eq!(paragraph_forward(&b, p(0, 0), 2), p(4, 0));
        assert_eq!(
            paragraph_forward(&b, p(5, 0), 1),
            p(5, 1),
            "no blank line after: buffer end"
        );
        assert_eq!(paragraph_backward(&b, p(5, 0), 1), p(4, 0));
        assert_eq!(paragraph_backward(&b, p(3, 0), 2), p(0, 0));
    }

    #[test]
    fn match_bracket_respects_nesting_and_crosses_lines() {
        let b = buf("f(a(b)c)\n{\n}");
        assert_eq!(
            match_bracket(&b, p(0, 0)),
            Some(p(0, 7)),
            "searches forward to the first bracket"
        );
        assert_eq!(match_bracket(&b, p(0, 7)), Some(p(0, 1)));
        assert_eq!(match_bracket(&b, p(0, 3)), Some(p(0, 5)));
        assert_eq!(match_bracket(&b, p(1, 0)), Some(p(2, 0)));
        assert_eq!(match_bracket(&buf("no brackets"), p(0, 0)), None);
    }

    // -- search -----------------------------------------------------------

    #[test]
    fn search_wraps_in_both_directions() {
        let b = buf("cat dog\ncat");
        let pat: Vec<char> = "cat".chars().collect();
        assert_eq!(search(&b, p(0, 0), &pat, false, false), Some(p(1, 0)));
        assert_eq!(
            search(&b, p(1, 0), &pat, false, false),
            Some(p(0, 0)),
            "wraps to the top"
        );
        assert_eq!(
            search(&b, p(0, 0), &pat, true, false),
            Some(p(1, 0)),
            "wraps to the bottom"
        );
    }

    #[test]
    fn search_uses_smartcase() {
        let b = buf("Hello hello");
        let lower: Vec<char> = "hello".chars().collect();
        let upper: Vec<char> = "Hello".chars().collect();
        assert_eq!(search(&b, p(0, 1), &lower, false, false), Some(p(0, 6)));
        assert_eq!(
            search(&b, p(0, 6), &lower, false, false),
            Some(p(0, 0)),
            "lowercase pattern matches either case"
        );
        assert_eq!(
            search(&b, p(0, 1), &upper, false, false),
            Some(p(0, 0)),
            "uppercase pattern is case-sensitive"
        );
    }

    #[test]
    fn whole_word_search_skips_partial_matches() {
        let b = buf("cat concat cat");
        let pat: Vec<char> = "cat".chars().collect();
        assert_eq!(search(&b, p(0, 0), &pat, false, true), Some(p(0, 11)));
        assert_eq!(search(&b, p(0, 0), &pat, false, false), Some(p(0, 7)));
    }

    #[test]
    fn search_returns_none_without_any_match() {
        let pat: Vec<char> = "zzz".chars().collect();
        assert_eq!(search(&buf("abc"), p(0, 0), &pat, false, false), None);
        assert_eq!(search(&buf("abc"), p(0, 0), &[], false, false), None);
    }

    // -- text objects -------------------------------------------------------

    fn span(r: Range) -> (Pos, Pos) {
        (r.start, r.end)
    }

    #[test]
    fn inner_and_around_word() {
        let b = buf("say hello there");
        assert_eq!(span(text_object(&b, p(0, 5), 'w', true).unwrap()), (p(0, 4), p(0, 9)));
        assert_eq!(
            span(text_object(&b, p(0, 5), 'w', false).unwrap()),
            (p(0, 4), p(0, 10)),
            "aw takes trailing space"
        );
        let last = text_object(&b, p(0, 12), 'w', false).unwrap();
        assert_eq!(span(last), (p(0, 9), p(0, 15)), "last word takes leading space instead");
        assert_eq!(text_object(&buf(""), p(0, 0), 'w', true), None);
    }

    #[test]
    fn around_word_on_whitespace_takes_the_following_word() {
        let b = buf("a  bc");
        assert_eq!(span(text_object(&b, p(0, 1), 'w', false).unwrap()), (p(0, 1), p(0, 5)));
    }

    #[test]
    fn paragraph_objects_are_linewise() {
        let b = buf("a\nb\n\nc");
        let ip = text_object(&b, p(0, 0), 'p', true).unwrap();
        assert!(ip.linewise);
        assert_eq!((ip.start.row, ip.end.row), (0, 1));
        let ap = text_object(&b, p(0, 0), 'p', false).unwrap();
        assert_eq!(
            (ap.start.row, ap.end.row),
            (0, 2),
            "ap includes the following blank line"
        );
        let last = text_object(&b, p(3, 0), 'p', false).unwrap();
        assert_eq!(
            (last.start.row, last.end.row),
            (2, 3),
            "nothing after: takes the blank line before"
        );
    }

    #[test]
    fn quote_objects_find_the_enclosing_or_next_pair() {
        let b = buf(r#"say "hi there" now"#);
        assert_eq!(span(text_object(&b, p(0, 6), '"', true).unwrap()), (p(0, 5), p(0, 13)));
        assert_eq!(span(text_object(&b, p(0, 6), '"', false).unwrap()), (p(0, 4), p(0, 15)));
        assert_eq!(
            span(text_object(&b, p(0, 0), '"', true).unwrap()),
            (p(0, 5), p(0, 13)),
            "cursor before: next pair"
        );
        assert_eq!(text_object(&buf("no quotes"), p(0, 0), '"', true), None);
    }

    #[test]
    fn bracket_objects_handle_nesting_and_every_alias() {
        let b = buf("f(a, (b), c)");
        assert_eq!(span(text_object(&b, p(0, 3), '(', true).unwrap()), (p(0, 2), p(0, 11)));
        assert_eq!(span(text_object(&b, p(0, 3), 'b', false).unwrap()), (p(0, 1), p(0, 12)));
        assert_eq!(
            span(text_object(&b, p(0, 6), ')', true).unwrap()),
            (p(0, 6), p(0, 7)),
            "innermost pair wins"
        );
        assert_eq!(
            span(text_object(&b, p(0, 11), '(', true).unwrap()),
            (p(0, 2), p(0, 11)),
            "cursor on the closer"
        );
        assert_eq!(
            span(text_object(&buf("[x]"), p(0, 1), '[', true).unwrap()),
            (p(0, 1), p(0, 2))
        );
        assert_eq!(
            span(text_object(&buf("{x}"), p(0, 1), 'B', false).unwrap()),
            (p(0, 0), p(0, 3))
        );
        assert_eq!(
            span(text_object(&buf("<x>"), p(0, 1), '<', true).unwrap()),
            (p(0, 1), p(0, 2))
        );
        assert_eq!(text_object(&buf("no brackets"), p(0, 0), '(', true), None);
    }

    #[test]
    fn bracket_object_on_an_inner_closer_selects_the_inner_pair() {
        let b = buf("((b))");
        assert_eq!(span(text_object(&b, p(0, 3), '(', false).unwrap()), (p(0, 1), p(0, 4)));
    }

    #[test]
    fn empty_bracket_pair_gives_an_empty_inner_range() {
        let r = text_object(&buf("()"), p(0, 0), '(', true).unwrap();
        assert_eq!(r.start, r.end);
    }
}
