use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use mail_core::{Address, Draft, Envelope, FlagKind, Folder, FolderRole, Message, OutboxCounts, SavedDraft};

use crate::attach::{self, AttachmentEntry, AttachmentSource};
use crate::editable::{TextArea, TextInput};
use crate::filebrowser::FileBrowser;
use crate::reply::{self, Respond};
use crate::vim::Vim;

/// Source of `ComposeState::id` — a fresh, process-wide-unique id per
/// compose session, so an async spellcheck result that outlives its
/// compose session (closed and a new one opened before the check
/// returned) can be told apart from the new one instead of silently
/// landing on it. `Relaxed` is enough: this only needs uniqueness
/// across calls, never ordering with anything else.
static NEXT_COMPOSE_ID: AtomicU64 = AtomicU64::new(0);

/// How long a status message (e.g. "Sent.") stays in the status bar
/// before it's cleared automatically, so the key hints underneath it
/// become visible again without needing another keypress.
pub const STATUS_MESSAGE_TTL: Duration = Duration::from_secs(5);

/// How many rows a vim-style `Ctrl-d`/`Ctrl-u` half-page jump moves.
pub const PAGE_JUMP: usize = 10;

/// How many fuzzy-matched sender suggestions to show at once under a
/// recipient field in compose.
pub const MAX_RECIPIENT_SUGGESTIONS: usize = 8;

/// The least time between two automatic saves of a compose's draft.
pub const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(5);

/// What a currently-open `FileBrowser` is for — decides what happens
/// when the user picks a file or confirms a save location.
pub enum BrowserPurpose {
    AttachToCompose,
    /// Which attachment of which message to save; the filename lives in
    /// the browser's own editable `filename` field (prefilled from it).
    /// The content is only extracted once a location is confirmed.
    SaveAttachment {
        uid: u32,
        index: usize,
    },
}

pub enum BodyState {
    Empty,
    /// A fetch for `uid` is in flight — its result is applied only while
    /// this is still the pending request (see `App::finish_body`).
    Loading {
        uid: u32,
    },
    /// Boxed: a parsed message is large next to the other states.
    Loaded(Box<Message>),
    Error(String),
}

/// The reading pane's size as of the last frame — recorded by rendering
/// (the only place that knows it) so scroll keys can clamp and page.
#[derive(Debug, Clone, Copy, Default)]
pub struct BodyViewport {
    /// Visible rows inside the border.
    pub height: u16,
    /// The furthest the content can scroll before its last row would
    /// leave the bottom of the pane.
    pub max_scroll: u16,
}

pub enum ListState {
    Loading,
    Loaded,
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeField {
    To,
    Cc,
    Bcc,
    Subject,
    Attachments,
    Body,
}

/// A list paired with which entry is currently highlighted — the shape
/// shared by compose's attachment list and its recipient-suggestion
/// dropdown. Grouping it once here keeps `ComposeState` from
/// re-deriving "list + selected index" bookkeeping (clamping on
/// removal, moving up/down) for each new feature that needs it.
pub struct Selectable<T> {
    pub items: Vec<T>,
    pub selected: usize,
}

impl<T> Default for Selectable<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            selected: 0,
        }
    }
}

impl<T> Selectable<T> {
    pub fn selected_item(&self) -> Option<&T> {
        self.items.get(self.selected)
    }

    /// Removes the highlighted item, clamping the selection back onto
    /// the last remaining one if it just fell out of bounds. A no-op on
    /// an empty list.
    pub fn remove_selected(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.items.remove(self.selected);
        if self.selected >= self.items.len() {
            self.selected = self.items.len().saturating_sub(1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1).min(self.items.len() - 1);
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.selected = 0;
    }
}

pub struct ComposeState {
    /// A fresh id per compose session (see `NEXT_COMPOSE_ID`), tagged
    /// onto every spellcheck request so a result that arrives after this
    /// session closed and a new one opened gets recognized as stale
    /// instead of silently applied to unrelated text.
    pub id: u64,
    pub to: TextInput,
    pub cc: TextInput,
    pub bcc: TextInput,
    pub subject: TextInput,
    pub body: TextArea,
    pub focus: ComposeField,
    pub attachments: Selectable<AttachmentEntry>,
    pub sending: bool,
    pub error: Option<String>,
    /// Fuzzy-matched sender suggestions for whatever's typed after the
    /// last comma in the currently focused recipient field (To/Cc/Bcc);
    /// empty hides the dropdown. Recomputed by `refresh_suggestions`.
    pub suggestions: Selectable<Address>,
    /// Misspelled words per body line, index-aligned with `body.lines`,
    /// each mapped to Hunspell's suggested corrections for it. Most
    /// edits patch just the edited line (see `body_change` and
    /// `main.rs`'s spellcheck wiring); an edit that changes the line
    /// count or touches several lines (`dd`, `J`, `p`, undo, ...)
    /// replaces the whole vec instead, since that can shift every later
    /// line's index. May be shorter than
    /// `body.lines` right after such an edit until the pending recheck
    /// result arrives — treated as "nothing flagged yet" rather than an
    /// error wherever it's read.
    pub misspelled_by_line: Vec<HashMap<String, Vec<String>>>,
    /// The body's modal (vim-style) editor state. Only consulted while the
    /// body has focus; entering the body starts it in Insert mode.
    pub vim: Vim,
    /// A one-shot message for the status line (vim errors, `:q` refusing
    /// to discard an unsent draft), cleared by the next key.
    pub notice: Option<String>,
    /// Set when no spellchecker could be started for this compose; shown
    /// in the body's title for as long as compose stays open.
    pub spellcheck_note: Option<String>,
    /// Identifies this compose's saved draft: every save replaces the
    /// draft stored under it. Carried over when a draft is resumed.
    pub draft_key: String,
    /// For a reply, the `Message-ID` it answers (see `Draft::in_reply_to`).
    pub in_reply_to: Option<String>,
    /// For a reply, the thread it joins (see `Draft::references`).
    pub references: Vec<String>,
    /// For a reply, the message it answers as `(folder_id, uid)` — marked
    /// answered once the reply is sent.
    pub answers: Option<(i64, u32)>,
    /// What was last saved as a draft (or handed off to be saved), to tell
    /// whether there's anything new to save.
    pub last_saved: Option<SavedDraft>,
    pub last_save_at: Option<Instant>,
}

impl ComposeState {
    pub fn blank() -> Self {
        Self {
            id: NEXT_COMPOSE_ID.fetch_add(1, Ordering::Relaxed),
            to: TextInput::default(),
            cc: TextInput::default(),
            bcc: TextInput::default(),
            subject: TextInput::default(),
            body: TextArea::default(),
            focus: ComposeField::To,
            attachments: Selectable::default(),
            sending: false,
            error: None,
            suggestions: Selectable::default(),
            misspelled_by_line: Vec::new(),
            vim: Vim::new(),
            notice: None,
            spellcheck_note: None,
            draft_key: new_draft_key(),
            in_reply_to: None,
            references: Vec::new(),
            answers: None,
            last_saved: None,
            last_save_at: None,
        }
    }

    /// A reply to, or forward of, `message` (from folder `folder_id`): its
    /// recipients, subject and quoted or forwarded text filled in, and for
    /// a reply the headers that keep it in its thread. A reply starts in
    /// Insert mode above the quote; a forward in the To field.
    pub fn respond(message: &Message, kind: Respond, own_email: &str, folder_id: i64) -> Self {
        let mut compose = Self::blank();
        match kind {
            Respond::Reply | Respond::ReplyAll => {
                let (to, cc) = reply::reply_recipients(message, own_email, kind == Respond::ReplyAll);
                compose.to = TextInput::with_value(reply::recipient_field(&to));
                compose.cc = TextInput::with_value(reply::recipient_field(&cc));
                compose.subject = TextInput::with_value(reply::reply_subject(&message.subject));
                compose.body.lines = reply::quoted_body(message);
                compose.in_reply_to = message.message_id.clone();
                compose.references = reply::thread_references(message);
                compose.answers = Some((folder_id, message.uid));
                compose.focus = ComposeField::Body;
                compose.vim.enter_insert(&compose.body);
            }
            Respond::Forward => {
                compose.subject = TextInput::with_value(reply::forward_subject(&message.subject));
                compose.body.lines = reply::forwarded_body(message);
                let mut total = 0;
                let mut left_out = 0;
                for (index, a) in message.attachments.iter().enumerate() {
                    if total + a.size_bytes > attach::MAX_TOTAL_ATTACHMENT_BYTES {
                        left_out += 1;
                        continue;
                    }
                    total += a.size_bytes;
                    compose.attachments.items.push(AttachmentEntry {
                        source: AttachmentSource::Message {
                            folder_id,
                            uid: message.uid,
                            index,
                        },
                        filename: a.filename.clone(),
                        size_bytes: a.size_bytes,
                    });
                }
                if left_out > 0 {
                    compose.notice = Some(format!(
                        "Left out {left_out} attachment(s) that would go over the {} limit",
                        attach::human_size(attach::MAX_TOTAL_ATTACHMENT_BYTES)
                    ));
                }
            }
        }
        compose
    }

    /// A saved draft, reopened where it left off — in the body, in Normal
    /// mode — and saved again under the same key.
    pub fn from_saved(saved: SavedDraft) -> Self {
        let mut compose = Self::blank();
        let draft = &saved.draft;
        compose.to = TextInput::with_value(draft.to.clone());
        compose.cc = TextInput::with_value(draft.cc.clone());
        compose.bcc = TextInput::with_value(draft.bcc.clone());
        compose.subject = TextInput::with_value(draft.subject.clone());
        compose.body.lines = draft.body.split('\n').map(str::to_string).collect();
        compose.attachments.items = attach::from_json(&saved.attachments);
        compose.in_reply_to = draft.in_reply_to.clone();
        compose.references = draft.references.clone();
        compose.answers = saved.answers;
        compose.draft_key = saved.key.clone();
        compose.focus_body_normal();
        compose.last_saved = Some(SavedDraft {
            updated_at: None,
            ..saved
        });
        compose
    }

    pub fn next_field(&mut self) {
        let from = self.focus;
        self.focus = match self.focus {
            ComposeField::To => ComposeField::Cc,
            ComposeField::Cc => ComposeField::Bcc,
            ComposeField::Bcc => ComposeField::Subject,
            ComposeField::Subject => ComposeField::Attachments,
            ComposeField::Attachments => ComposeField::Body,
            ComposeField::Body => ComposeField::To,
        };
        self.focus_changed_from(from);
    }

    pub fn prev_field(&mut self) {
        let from = self.focus;
        self.focus = match self.focus {
            ComposeField::To => ComposeField::Body,
            ComposeField::Cc => ComposeField::To,
            ComposeField::Bcc => ComposeField::Cc,
            ComposeField::Subject => ComposeField::Bcc,
            ComposeField::Attachments => ComposeField::Subject,
            ComposeField::Body => ComposeField::Attachments,
        };
        self.focus_changed_from(from);
    }

    /// Esc from a header field: jump to the body in Normal mode, so `:q`
    /// / `:wq` are always one keystroke away and Esc never discards.
    pub fn focus_body_normal(&mut self) {
        self.focus = ComposeField::Body;
        self.vim.enter_normal(&mut self.body);
    }

    /// Keeps the editor's mode in step with focus: arriving in the body
    /// starts an Insert session (typing just works), leaving it finishes
    /// whatever was in progress so it lands in the undo history.
    fn focus_changed_from(&mut self, from: ComposeField) {
        let body = ComposeField::Body;
        if from == body && self.focus != body {
            self.vim.leave(&self.body);
        } else if from != body && self.focus == body {
            self.vim.enter_insert(&self.body);
        }
    }

    /// Whether there's nothing in this compose worth keeping — `:q` only
    /// closes a blank one (`:q!` closes anything).
    pub fn is_blank(&self) -> bool {
        [&self.to, &self.cc, &self.bcc, &self.subject]
            .iter()
            .all(|f| f.value.trim().is_empty())
            && self.body.lines.iter().all(|l| l.trim().is_empty())
            && self.attachments.items.is_empty()
    }

    pub fn to_draft(&self) -> Draft {
        Draft {
            to: self.to.value.clone(),
            cc: self.cc.value.clone(),
            bcc: self.bcc.value.clone(),
            subject: self.subject.value.clone(),
            body: self.body.text(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
        }
    }

    /// Everything a saved draft holds, as of now.
    pub fn to_saved(&self) -> SavedDraft {
        SavedDraft {
            key: self.draft_key.clone(),
            draft: self.to_draft(),
            attachments: attach::to_json(&self.attachments.items),
            answers: self.answers,
            updated_at: None,
        }
    }

    /// Whether there's anything a saved draft doesn't have yet.
    pub fn has_unsaved_changes(&self) -> bool {
        !self.is_blank() && self.last_saved.as_ref() != Some(&self.to_saved())
    }

    /// The draft to save automatically, if one is due: something changed
    /// since the last save, and that was at least `AUTOSAVE_INTERVAL` ago.
    /// Recorded as saved right away, so it isn't handed out twice.
    pub fn autosave_due(&mut self, now: Instant) -> Option<SavedDraft> {
        if self.sending
            || self
                .last_save_at
                .is_some_and(|at| now.duration_since(at) < AUTOSAVE_INTERVAL)
        {
            return None;
        }
        self.has_unsaved_changes().then(|| self.take_save(now))
    }

    /// The draft to save now (`:w`, closing), recorded as saved.
    pub fn take_save(&mut self, now: Instant) -> SavedDraft {
        let saved = self.to_saved();
        self.last_saved = Some(saved.clone());
        self.last_save_at = Some(now);
        saved
    }

    /// A save didn't go through: forget it, so the next autosave retries.
    pub fn save_failed(&mut self) {
        self.last_saved = None;
    }

    pub fn attachments_total_bytes(&self) -> u64 {
        self.attachments.items.iter().map(|a| a.size_bytes).sum()
    }

    /// Recomputes `suggestions` against whatever's typed after the last
    /// recipient separator in the currently focused field — call after
    /// every edit or focus change. Clears immediately outside To/Cc/Bcc,
    /// or when that fragment is empty (nothing to suggest against yet).
    pub fn refresh_suggestions(&mut self, known_senders: &[Address]) {
        self.suggestions.selected = 0;
        let field_value = match self.focus {
            ComposeField::To => &self.to.value,
            ComposeField::Cc => &self.cc.value,
            ComposeField::Bcc => &self.bcc.value,
            ComposeField::Subject | ComposeField::Attachments | ComposeField::Body => {
                self.suggestions.items.clear();
                return;
            }
        };
        let query = match last_recipient_separator(field_value) {
            Some(idx) => field_value[idx + 1..].trim(),
            None => field_value.trim(),
        };
        self.suggestions.items = if query.is_empty() {
            Vec::new()
        } else {
            crate::fuzzy::best_matches(query, known_senders, MAX_RECIPIENT_SUGGESTIONS)
        };
    }

    /// Accepts the highlighted suggestion into the focused recipient
    /// field, replacing whatever was typed after the last recipient
    /// separator and leaving a trailing ", " ready for the next address.
    /// A no-op outside To/Cc/Bcc, or when there's nothing selected to
    /// accept.
    pub fn accept_suggestion(&mut self) {
        let Some(chosen) = self.suggestions.selected_item().cloned() else {
            return;
        };
        let field = match self.focus {
            ComposeField::To => &mut self.to,
            ComposeField::Cc => &mut self.cc,
            ComposeField::Bcc => &mut self.bcc,
            ComposeField::Subject | ComposeField::Attachments | ComposeField::Body => return,
        };
        let prefix = match last_recipient_separator(&field.value) {
            Some(idx) => format!("{} ", field.value[..=idx].trim_end()),
            None => String::new(),
        };
        // Quoted where needed, so a name like "Doe, Jane" stays one recipient.
        field.value = format!("{prefix}{}, ", chosen.to_recipient());
        field.cursor = field.value.chars().count();
        self.suggestions.clear();
    }

    /// Replaces one body line's flagged words — the common case, since
    /// most edits (typing or backspacing within a line) don't change the
    /// line count. Grows the vec if needed rather than requiring it to
    /// already be exactly the right length.
    pub fn set_line_misspellings(&mut self, line: usize, words: HashMap<String, Vec<String>>) {
        if line >= self.misspelled_by_line.len() {
            self.misspelled_by_line.resize_with(line + 1, HashMap::new);
        }
        self.misspelled_by_line[line] = words;
    }

    /// Replaces every line's flagged words at once, in line order — used
    /// after an edit that changes the line count (Enter, or a
    /// line-joining Backspace), since that shifts every later line's
    /// index and a single-line patch could land on the wrong line.
    pub fn set_all_misspellings(&mut self, by_line: Vec<HashMap<String, Vec<String>>>) {
        self.misspelled_by_line = by_line;
    }

    /// The misspelled word the cursor is currently on or just after in
    /// the body, if any, paired with Hunspell's suggested corrections for
    /// it — what the "did you mean?" popup (see `ui/compose.rs`) needs to
    /// know whether to show and what to list. `None` when the body isn't
    /// focused (stale cursor position), the cursor isn't on a word, or
    /// that word isn't flagged.
    pub fn misspelled_word_at_cursor(&self) -> Option<(&str, &[String])> {
        if self.focus != ComposeField::Body {
            return None;
        }
        let line = self.body.lines.get(self.body.cursor_row)?;
        let word = crate::spellcheck::word_at_cursor(line, self.body.cursor_col)?;
        let key = crate::spellcheck::normalize_word(word);
        let suggestions = self.misspelled_by_line.get(self.body.cursor_row)?.get(&key)?;
        Some((word, suggestions.as_slice()))
    }
}

pub struct App {
    /// Every configured account's email, in the fixed order they were
    /// loaded at startup — index-matched with the `Accounts` list main.rs
    /// owns. Doesn't change after startup; only `current_account` does.
    pub account_emails: Vec<String>,
    pub current_account: usize,
    /// Each account's folders in sidebar order, index-matched with
    /// `account_emails`. Empty until first read from the cache.
    pub folders: Vec<Vec<Folder>>,
    /// The folder whose messages are shown.
    pub current_folder_id: i64,
    /// A reply or forward waiting for its message's body to load, since
    /// quoting and threading need it: `(uid, what to do)`.
    pub pending_respond: Option<(u32, Respond)>,
    /// The current account's queued (not yet sent) messages.
    pub outbox: OutboxCounts,
    /// The saved-drafts picker, while it's open.
    pub drafts: Option<Selectable<SavedDraft>>,
    pub list_state: ListState,
    /// The current account's cached envelopes, newest first. Private so
    /// every change goes through a method that keeps `visible` and
    /// `selected` consistent with it — see `replace_envelopes`.
    envelopes: Vec<Envelope>,
    /// Index into `visible` (not into `envelopes`): the highlighted row.
    pub selected: usize,
    pub body: BodyState,
    /// How many lines the reading pane is scrolled down. Reset whenever a
    /// different message is opened.
    pub body_scroll: u16,
    /// Written by the reading pane each frame; see `BodyViewport`.
    pub body_viewport: Cell<BodyViewport>,
    /// Which of the open message's attachments is highlighted for
    /// download — reset to 0 whenever a new message body loads.
    pub selected_attachment: usize,
    pub compose: Option<ComposeState>,
    /// The address book fuzzy-matched against while typing a recipient in
    /// compose: every sender the current account has received mail from,
    /// minus no-reply senders. Loaded asynchronously (see `bg.rs`'s
    /// `spawn_known_senders`), so it may briefly be empty right after
    /// startup or an account switch.
    pub known_senders: Vec<Address>,
    pub status_message: Option<(String, Instant)>,
    /// `Some` whenever a filter is showing (being edited or confirmed).
    /// An empty query is equivalent to no filter. Private for the same
    /// reason as `envelopes`: editing it must refresh `visible`.
    search: Option<TextInput>,
    pub search_editing: bool,
    /// Indices into `envelopes` that match the active search, in display
    /// order. Cached rather than recomputed on demand — with up to
    /// `DISPLAY_LIMIT` envelopes and a filter active, recomputing means
    /// lowercasing thousands of subjects, several times per key and
    /// frame. Refreshed by every method that changes `envelopes` or
    /// `search`.
    visible: Vec<usize>,
    /// A modal directory browser, open either from compose (attach) or
    /// the reading pane (save an attachment). Rendered as one overlay
    /// regardless of which triggered it.
    pub file_browser: Option<(FileBrowser, BrowserPurpose)>,
    /// `Some(uid)` while asking "delete this message? [y/N]" — set by
    /// pressing `d`, resolved by the next keypress (`y`/`Y` confirms,
    /// anything else cancels).
    pub confirm_delete: Option<u32>,
    /// Whether there's reason to believe older mail than what's cached
    /// still exists on the server — starts `true`, flips to `false` once
    /// a "load more" request comes back empty (i.e. we've reached the
    /// real start of the mailbox), so scrolling to the bottom stops
    /// trying.
    pub has_more_older: bool,
    /// Guards against firing multiple concurrent "load more" requests
    /// while one is already in flight.
    pub loading_more: bool,
    /// Set after a lone `g` keypress in normal mode, waiting to see if the
    /// next key completes vim's `gg` ("jump to top"); cleared by any other
    /// key.
    pub pending_g: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new(account_emails: Vec<String>) -> Self {
        Self {
            folders: vec![Vec::new(); account_emails.len()],
            account_emails,
            current_account: 0,
            current_folder_id: 0,
            pending_respond: None,
            outbox: OutboxCounts::default(),
            drafts: None,
            list_state: ListState::Loading,
            envelopes: Vec::new(),
            selected: 0,
            body: BodyState::Empty,
            body_scroll: 0,
            body_viewport: Cell::new(BodyViewport::default()),
            selected_attachment: 0,
            compose: None,
            known_senders: Vec::new(),
            status_message: None,
            search: None,
            search_editing: false,
            visible: Vec::new(),
            file_browser: None,
            has_more_older: true,
            loading_more: false,
            pending_g: false,
            confirm_delete: None,
            should_quit: false,
        }
    }

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status_message = Some((message.into(), Instant::now()));
    }

    pub fn current_account_email(&self) -> &str {
        &self.account_emails[self.current_account]
    }

    /// Resets everything that's specific to whichever account was
    /// previously active — called right after `current_account` changes,
    /// before the new account's cached envelopes are loaded in.
    pub fn reset_for_account_switch(&mut self, inbox_id: i64) {
        self.show_folder(inbox_id);
        self.known_senders.clear();
        self.outbox = OutboxCounts::default();
    }

    /// Switches the list to another folder (of the current account):
    /// everything about the old folder's list goes.
    pub fn show_folder(&mut self, folder_id: i64) {
        self.current_folder_id = folder_id;
        self.envelopes.clear();
        self.visible.clear();
        self.selected = 0;
        self.selected_attachment = 0;
        self.body = BodyState::Empty;
        self.body_scroll = 0;
        self.list_state = ListState::Loading;
        self.clear_search();
        self.has_more_older = true;
        self.loading_more = false;
        self.pending_g = false;
        self.pending_respond = None;
        self.confirm_delete = None;
    }

    // -- folders --------------------------------------------------------------

    pub fn set_folders(&mut self, account: usize, folders: Vec<Folder>) {
        if let Some(slot) = self.folders.get_mut(account) {
            *slot = folders;
        }
    }

    pub fn current_folder(&self) -> Option<&Folder> {
        self.folders
            .get(self.current_account)?
            .iter()
            .find(|f| f.id == self.current_folder_id)
    }

    pub fn current_folder_role(&self) -> Option<FolderRole> {
        self.current_folder().and_then(|f| f.role)
    }

    /// The selectable folder before (`forward == false`) or after the
    /// current one in the sidebar, wrapping around.
    pub fn neighbor_folder(&self, forward: bool) -> Option<i64> {
        let folders: Vec<&Folder> = self
            .folders
            .get(self.current_account)?
            .iter()
            .filter(|f| f.selectable)
            .collect();
        if folders.is_empty() {
            return None;
        }
        let len = folders.len();
        let next = match folders.iter().position(|f| f.id == self.current_folder_id) {
            Some(i) if forward => (i + 1) % len,
            Some(i) => (i + len - 1) % len,
            None => 0,
        };
        Some(folders[next].id).filter(|&id| id != self.current_folder_id)
    }

    /// Changes the current folder's unread count by `delta` — keeps the
    /// sidebar in step with reads and toggles until the next refresh
    /// brings the server's count.
    fn adjust_unseen(&mut self, delta: i64) {
        let (account, folder_id) = (self.current_account, self.current_folder_id);
        if let Some(folder) = self
            .folders
            .get_mut(account)
            .and_then(|list| list.iter_mut().find(|f| f.id == folder_id))
        {
            if let Some(unseen) = &mut folder.unseen {
                *unseen = (i64::from(*unseen) + delta).max(0) as u32;
            }
        }
    }

    /// Sets a flag on a listed message, returning whether it changed.
    pub fn set_envelope_flag(&mut self, uid: u32, flag: FlagKind, on: bool) -> bool {
        let Some(envelope) = self.envelopes.iter_mut().find(|e| e.uid == uid) else {
            return false;
        };
        if envelope.flags.get(flag) == on {
            return false;
        }
        envelope.flags.set(flag, on);
        if flag == FlagKind::Seen {
            self.adjust_unseen(if on { -1 } else { 1 });
        }
        true
    }

    /// Clears the status message once its TTL has elapsed; called from
    /// the event loop's periodic tick. Returns the remaining duration
    /// until it should next be checked (used to size that tick), or
    /// `None` when there's nothing showing.
    pub fn expire_status(&mut self) -> Option<Duration> {
        let (_, set_at) = self.status_message.as_ref()?;
        let elapsed = set_at.elapsed();
        if elapsed >= STATUS_MESSAGE_TTL {
            self.status_message = None;
            None
        } else {
            Some(STATUS_MESSAGE_TTL - elapsed)
        }
    }

    // -- envelope list ------------------------------------------------------

    pub fn envelopes(&self) -> &[Envelope] {
        &self.envelopes
    }

    /// Indices into `envelopes()` that match the active search query, in
    /// display order — all of them when there's no query.
    pub fn visible_indices(&self) -> &[usize] {
        &self.visible
    }

    fn recompute_visible(&mut self) {
        self.visible = match self.search_query() {
            Some(query) => {
                let needle = query.to_lowercase();
                self.envelopes
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| envelope_matches(e, &needle))
                    .map(|(i, _)| i)
                    .collect()
            }
            None => (0..self.envelopes.len()).collect(),
        };
    }

    /// Swaps in a freshly read envelope list (a sync, a cache re-read after
    /// new mail or a backfill, ...) while keeping the cursor on the same
    /// *message* rather than the same row index — new mail lands at the
    /// top, so an unchanged index would silently point `r`/`Enter` at a
    /// different message. Falls back to the same row (clamped) if the
    /// selected message is gone.
    pub fn replace_envelopes(&mut self, envelopes: Vec<Envelope>) {
        let selected_uid = self.selected_uid();
        self.envelopes = envelopes;
        self.recompute_visible();
        self.reselect(selected_uid);
    }

    /// Adds server-side search results that aren't already in the list,
    /// keeping it newest-first and the selection on the same message.
    /// Returns how many were actually new.
    pub fn merge_envelopes(&mut self, extra: Vec<Envelope>) -> usize {
        let existing: std::collections::HashSet<u32> = self.envelopes.iter().map(|e| e.uid).collect();
        let new: Vec<Envelope> = extra.into_iter().filter(|e| !existing.contains(&e.uid)).collect();
        let added = new.len();
        if added > 0 {
            let selected_uid = self.selected_uid();
            self.envelopes.extend(new);
            self.envelopes.sort_by_key(|e| std::cmp::Reverse(e.uid));
            self.recompute_visible();
            self.reselect(selected_uid);
        }
        added
    }

    /// Drops a deleted message from view and keeps `selected` in bounds,
    /// clearing the reading pane too if that's the message it was showing.
    pub fn remove_envelope(&mut self, uid: u32) {
        self.envelopes.retain(|e| e.uid != uid);
        self.recompute_visible();
        self.clamp_selection();

        if self.body_uid() == Some(uid) {
            self.body = BodyState::Empty;
        }
    }

    /// Puts the cursor back on `uid` if it's still visible, otherwise
    /// keeps the current row (clamped to the new list's bounds).
    fn reselect(&mut self, uid: Option<u32>) {
        let found = uid.and_then(|uid| self.visible.iter().position(|&i| self.envelopes[i].uid == uid));
        match found {
            Some(row) => self.selected = row,
            None => self.clamp_selection(),
        }
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
    }

    /// Applies a finished sync's freshly re-read list. A failure leaves the
    /// cached list in place (offline reading must keep working) and only
    /// records the error for the title and status bar.
    pub fn apply_sync_result(&mut self, result: Result<Vec<Envelope>, String>) {
        match result {
            Ok(envelopes) => {
                self.replace_envelopes(envelopes);
                self.list_state = ListState::Loaded;
            }
            Err(e) => {
                self.set_status(format!("Sync failed: {e}"));
                self.list_state = ListState::Error(e);
            }
        }
    }

    /// Applies a plain cache re-read (account switch, new mail, a backfill).
    /// Only ever upgrades `Loading` to `Loaded` — it isn't evidence that a
    /// previously failed sync has recovered.
    pub fn apply_cached_envelopes(&mut self, envelopes: Vec<Envelope>) {
        let empty = envelopes.is_empty();
        self.replace_envelopes(envelopes);
        if !empty && matches!(self.list_state, ListState::Loading) {
            self.list_state = ListState::Loaded;
        }
    }

    // -- selection ------------------------------------------------------------

    pub fn select_next(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        self.selected = (self.selected + 1).min(self.visible.len() - 1);
    }

    /// Whether the selection is on the last visible row — the trigger
    /// point for "scroll to the bottom to load more older mail". Only
    /// meaningful (and only checked by the caller) with no search filter
    /// active, since "end of the filtered results" isn't the same thing
    /// as "end of what's cached".
    pub fn is_at_end_of_list(&self) -> bool {
        !self.visible.is_empty() && self.selected + 1 >= self.visible.len()
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Vim-style `gg` — jump to the first row.
    pub fn select_top(&mut self) {
        self.selected = 0;
    }

    /// Vim-style `G` — jump to the last visible row.
    pub fn select_bottom(&mut self) {
        self.selected = self.visible.len().saturating_sub(1);
    }

    /// Vim-style `Ctrl-d` — jump `PAGE_JUMP` rows down, clamped to the
    /// last visible row.
    pub fn select_page_down(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        self.selected = (self.selected + PAGE_JUMP).min(self.visible.len() - 1);
    }

    /// Vim-style `Ctrl-u` — jump `PAGE_JUMP` rows up, clamped to the top.
    pub fn select_page_up(&mut self) {
        self.selected = self.selected.saturating_sub(PAGE_JUMP);
    }

    pub fn selected_uid(&self) -> Option<u32> {
        self.selected_envelope().map(|e| e.uid)
    }

    pub fn selected_envelope(&self) -> Option<&Envelope> {
        self.visible.get(self.selected).and_then(|&i| self.envelopes.get(i))
    }

    // -- reading pane -----------------------------------------------------------

    /// The message the reading pane is showing or fetching, if any.
    pub fn body_uid(&self) -> Option<u32> {
        match &self.body {
            BodyState::Loading { uid } => Some(*uid),
            BodyState::Loaded(message) => Some(message.uid),
            BodyState::Empty | BodyState::Error(_) => None,
        }
    }

    /// Scrolls the reading pane by `lines` (negative is up), clamped to
    /// the content. A no-op unless a message is open.
    pub fn scroll_body(&mut self, lines: i32) {
        if !matches!(self.body, BodyState::Loaded(_)) {
            return;
        }
        let max = i32::from(self.body_viewport.get().max_scroll);
        self.body_scroll = (i32::from(self.body_scroll) + lines).clamp(0, max) as u16;
    }

    /// Scrolls the reading pane by one screenful, keeping one line of
    /// overlap for context.
    pub fn page_body(&mut self, down: bool) {
        let page = i32::from(self.body_viewport.get().height.saturating_sub(1).max(1));
        self.scroll_body(if down { page } else { -page });
    }

    pub fn start_loading_body(&mut self, uid: u32) {
        // A reply waiting for another message's body won't get it now.
        if self.pending_respond.is_some_and(|(pending, _)| pending != uid) {
            self.pending_respond = None;
        }
        self.body = BodyState::Loading { uid };
        self.body_scroll = 0;
    }

    /// Applies a finished body fetch — but only if it's the one the reading
    /// pane is still waiting for. Keyed on the *requested* uid rather than
    /// the current selection: moving the cursor while a fetch is in flight
    /// used to drop the result and leave "Loading..." up forever.
    pub fn finish_body(&mut self, uid: u32, result: Result<Message, String>) {
        if !matches!(self.body, BodyState::Loading { uid: pending } if pending == uid) {
            return;
        }
        match result {
            Ok(message) => {
                // mail-core marked it read in the cache (and on the server);
                // mirror that in the list right away.
                self.set_envelope_flag(uid, FlagKind::Seen, true);
                self.selected_attachment = 0;
                self.body = BodyState::Loaded(Box::new(message));
            }
            Err(e) => {
                self.body = BodyState::Error(e);
                if self.pending_respond.is_some_and(|(pending, _)| pending == uid) {
                    self.pending_respond = None;
                }
            }
        }
    }

    // -- search ---------------------------------------------------------------

    pub fn search(&self) -> Option<&TextInput> {
        self.search.as_ref()
    }

    /// The active filter, trimmed — `None` when there's no search or it's
    /// blank (which filters nothing).
    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(|s| s.value.trim()).filter(|q| !q.is_empty())
    }

    pub fn start_search(&mut self) {
        self.search = Some(TextInput::default());
        self.search_editing = true;
        self.selected = 0;
        self.recompute_visible();
    }

    /// Applies an edit to the search field, refreshing the filter and
    /// jumping back to the first match if the query actually changed (a
    /// cursor move within the field leaves the selection alone).
    pub fn edit_search(&mut self, op: impl FnOnce(&mut TextInput)) {
        let Some(search) = &mut self.search else { return };
        let before = search.value.clone();
        op(search);
        if search.value != before {
            self.selected = 0;
            self.recompute_visible();
        }
    }

    pub fn clear_search(&mut self) {
        self.search = None;
        self.search_editing = false;
        self.selected = 0;
        self.recompute_visible();
    }

    /// Sets the filter outright — the test-side counterpart of typing it.
    #[cfg(test)]
    pub fn set_search_query(&mut self, query: &str) {
        self.search = Some(TextInput::with_value(query));
        self.recompute_visible();
        self.clamp_selection();
    }
}

/// Which body lines an edit touched, as far as spellcheck cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyChange {
    None,
    /// Exactly one line changed and the line count didn't: recheck just it.
    Line(usize),
    /// The line count changed or several lines did: recheck everything,
    /// since per-line indices may have shifted.
    Full,
}

/// Diffs the body before and after a key, so spellcheck rechecks exactly
/// what an edit touched — whatever vim command made it (or undid it).
pub fn body_change(before: &[String], after: &[String]) -> BodyChange {
    if before.len() != after.len() {
        return BodyChange::Full;
    }
    let mut changed = before
        .iter()
        .zip(after)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, _)| i);
    match (changed.next(), changed.next()) {
        (None, _) => BodyChange::None,
        (Some(row), None) => BodyChange::Line(row),
        _ => BodyChange::Full,
    }
}

/// The byte index of the last comma in `s` that separates two recipients
/// in a To/Cc/Bcc field, ignoring one inside a quoted display name
/// (`"Doe, Jane" <jane@example.com>`). An odd number of `"` before a
/// comma means it's still inside an (possibly unterminated, still being
/// typed) quoted name, so that comma doesn't count as a separator either.
fn last_recipient_separator(s: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut last = None;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => last = Some(i),
            _ => {}
        }
    }
    last
}

/// A key for a new compose session's draft, unique across runs.
fn new_draft_key() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn envelope_matches(e: &Envelope, needle: &str) -> bool {
    e.subject.to_lowercase().contains(needle)
        || e.from.iter().any(|a| {
            a.email.to_lowercase().contains(needle) || a.name.as_deref().unwrap_or("").to_lowercase().contains(needle)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_core::types::{Address, Flags};

    fn env(uid: u32, subject: &str, from_name: &str, from_email: &str) -> Envelope {
        Envelope {
            uid,
            subject: subject.to_string(),
            from: vec![Address {
                name: Some(from_name.to_string()),
                email: from_email.to_string(),
            }],
            to: Vec::new(),
            date: None,
            flags: Flags::default(),
        }
    }

    fn app_with(envelopes: Vec<Envelope>) -> App {
        let mut app = App::new(vec!["me@example.com".to_string()]);
        app.replace_envelopes(envelopes);
        app
    }

    fn sample_app() -> App {
        app_with(vec![
            env(5, "Weekly digest", "News", "news@example.com"),
            env(4, "Re: project plan", "Alice", "alice@example.com"),
            env(3, "Invoice #42", "Billing", "billing@example.com"),
            env(2, "Lunch?", "Bob", "bob@example.com"),
            env(1, "Welcome", "Support", "support@example.com"),
        ])
    }

    // -- visible_indices / search filtering --------------------------------

    #[test]
    fn visible_indices_with_no_search_returns_everything_in_order() {
        let app = sample_app();
        assert_eq!(app.visible_indices(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn visible_indices_filters_by_subject_case_insensitively() {
        let mut app = sample_app();
        app.set_search_query("invoice");
        assert_eq!(app.visible_indices(), vec![2]);
    }

    #[test]
    fn visible_indices_filters_by_sender_name_or_email() {
        let mut app = sample_app();
        app.set_search_query("alice");
        assert_eq!(app.visible_indices(), vec![1]);

        app.set_search_query("bob@example.com");
        assert_eq!(app.visible_indices(), vec![3]);
    }

    #[test]
    fn visible_indices_treats_blank_or_whitespace_query_as_no_filter() {
        let mut app = sample_app();
        app.set_search_query("   ");
        assert_eq!(app.visible_indices(), vec![0, 1, 2, 3, 4]);
    }

    // -- selection movement --------------------------------------------------

    #[test]
    fn select_next_and_prev_clamp_at_the_list_bounds() {
        let mut app = sample_app();
        assert_eq!(app.selected, 0);
        app.select_prev();
        assert_eq!(app.selected, 0, "select_prev must not go below 0");

        for _ in 0..10 {
            app.select_next();
        }
        assert_eq!(app.selected, 4, "select_next must clamp at the last row");
    }

    #[test]
    fn select_next_and_prev_on_an_empty_list_do_nothing() {
        let mut app = app_with(Vec::new());
        app.select_next();
        assert_eq!(app.selected, 0);
        app.select_prev();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn select_top_and_bottom_jump_to_the_list_ends() {
        let mut app = sample_app();
        app.selected = 2;
        app.select_bottom();
        assert_eq!(app.selected, 4);
        app.select_top();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn select_bottom_on_an_empty_list_stays_at_zero() {
        let mut app = app_with(Vec::new());
        app.select_bottom();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn select_page_down_and_up_jump_by_page_jump_and_clamp() {
        let mut app = sample_app();
        app.select_page_down();
        assert_eq!(
            app.selected, 4,
            "only 5 rows exist, so a 10-row jump clamps to the last one"
        );

        app.select_page_up();
        assert_eq!(app.selected, 0, "a 10-row jump back up from row 4 clamps to the top");
    }

    #[test]
    fn select_page_down_on_an_empty_list_does_nothing() {
        let mut app = app_with(Vec::new());
        app.select_page_down();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn is_at_end_of_list_is_true_only_on_the_last_row_of_a_nonempty_list() {
        let mut app = sample_app();
        assert!(!app.is_at_end_of_list());
        app.selected = 4;
        assert!(app.is_at_end_of_list());

        let empty = app_with(Vec::new());
        assert!(!empty.is_at_end_of_list(), "an empty list has no 'end' to reach");
    }

    // -- selected_uid / selected_envelope respect the active filter ---------

    #[test]
    fn selected_uid_maps_through_the_active_filter() {
        let mut app = sample_app();
        app.set_search_query("a"); // matches several
        let visible = app.visible_indices();
        assert!(!visible.is_empty());
        assert_eq!(app.selected_uid(), Some(app.envelopes()[visible[0]].uid));
    }

    #[test]
    fn selected_uid_is_none_when_filter_matches_nothing() {
        let mut app = sample_app();
        app.set_search_query("zzz_no_match");
        assert_eq!(app.selected_uid(), None);
    }

    // -- remove_envelope ------------------------------------------------------

    #[test]
    fn remove_envelope_drops_the_row_and_keeps_selection_in_bounds() {
        let mut app = sample_app();
        app.selected = 4; // last row (uid 1)
        app.remove_envelope(1);

        assert_eq!(app.envelopes().len(), 4);
        assert!(app.envelopes().iter().all(|e| e.uid != 1));
        assert_eq!(app.selected, 3, "selection must move back onto the new last row");
    }

    #[test]
    fn remove_envelope_clears_the_reading_pane_if_it_was_showing_that_message() {
        let mut app = sample_app();
        app.body = BodyState::Loaded(Box::new(Message {
            uid: 3,
            subject: "Invoice #42".to_string(),
            ..Message::default()
        }));

        app.remove_envelope(3);

        assert!(matches!(app.body, BodyState::Empty));
    }

    #[test]
    fn remove_envelope_leaves_the_reading_pane_alone_for_a_different_message() {
        let mut app = sample_app();
        app.body = BodyState::Loaded(Box::new(Message {
            uid: 3,
            subject: "Invoice #42".to_string(),
            ..Message::default()
        }));

        app.remove_envelope(1);

        assert!(matches!(app.body, BodyState::Loaded(_)));
    }

    // -- replace_envelopes / merge_envelopes keep the selection by uid ---------

    #[test]
    fn new_mail_arriving_at_the_top_keeps_the_cursor_on_the_same_message() {
        let mut app = sample_app();
        app.selected = 2; // uid 3
        let mut refreshed = vec![
            env(7, "Newer", "X", "x@example.com"),
            env(6, "New", "Y", "y@example.com"),
        ];
        refreshed.extend(app.envelopes().to_vec());

        app.replace_envelopes(refreshed);

        assert_eq!(
            app.selected_uid(),
            Some(3),
            "the cursor must follow the message, not the row index"
        );
        assert_eq!(app.selected, 4);
    }

    #[test]
    fn replace_envelopes_maps_the_selection_through_an_active_filter() {
        let mut app = sample_app();
        app.set_search_query("o"); // matches several, in list order
        let target = app.visible_indices()[1];
        let target_uid = app.envelopes()[target].uid;
        app.selected = 1;

        let mut refreshed = vec![env(9, "Something new", "Bob", "bob@example.com")];
        refreshed.extend(app.envelopes().to_vec());
        app.replace_envelopes(refreshed);

        assert_eq!(
            app.selected_uid(),
            Some(target_uid),
            "the selection is a row in the *filtered* list and must stay on the same message"
        );
    }

    #[test]
    fn replace_envelopes_clamps_when_the_selected_message_disappeared() {
        let mut app = sample_app();
        app.selected = 4; // uid 1, last row
        app.replace_envelopes(app.envelopes()[..3].to_vec());
        assert_eq!(app.selected, 2, "falls back to the nearest remaining row");
    }

    #[test]
    fn merge_envelopes_adds_only_new_uids_sorted_and_keeps_the_selection() {
        let mut app = sample_app();
        app.selected = 1; // uid 4
        let added = app.merge_envelopes(vec![
            env(4, "Re: project plan", "Alice", "alice@example.com"), // already there
            env(10, "Older?", "Zed", "zed@example.com"),
        ]);

        assert_eq!(added, 1);
        let uids: Vec<u32> = app.envelopes().iter().map(|e| e.uid).collect();
        assert_eq!(uids, vec![10, 5, 4, 3, 2, 1]);
        assert_eq!(app.selected_uid(), Some(4));
    }

    // -- sync results ---------------------------------------------------------

    #[test]
    fn a_failed_sync_keeps_the_cached_list_and_reports_the_error() {
        let mut app = sample_app();
        app.list_state = ListState::Loaded;

        app.apply_sync_result(Err("connection refused".to_string()));

        assert_eq!(
            app.envelopes().len(),
            5,
            "offline reading of the cache must keep working"
        );
        assert!(matches!(app.list_state, ListState::Error(_)));
        assert!(app.status_message.as_ref().unwrap().0.contains("connection refused"));
    }

    #[test]
    fn a_cache_reread_does_not_clear_a_sync_error() {
        let mut app = sample_app();
        app.apply_sync_result(Err("offline".to_string()));
        app.apply_cached_envelopes(app.envelopes().to_vec());
        assert!(matches!(app.list_state, ListState::Error(_)));
    }

    #[test]
    fn a_nonempty_cache_read_upgrades_loading_to_loaded() {
        let mut app = App::new(vec!["me@example.com".to_string()]);
        app.apply_cached_envelopes(Vec::new());
        assert!(
            matches!(app.list_state, ListState::Loading),
            "an empty cache still waits for the sync"
        );
        app.apply_cached_envelopes(vec![env(1, "Hi", "A", "a@example.com")]);
        assert!(matches!(app.list_state, ListState::Loaded));
    }

    // -- body fetch results -----------------------------------------------------

    #[test]
    fn a_body_result_still_lands_after_the_selection_moved_on() {
        let mut app = sample_app();
        app.start_loading_body(5);
        app.select_next(); // cursor moves away while the fetch is in flight

        app.finish_body(5, Ok(message(5)));

        assert!(matches!(&app.body, BodyState::Loaded(m) if m.uid == 5));
    }

    #[test]
    fn opening_a_message_marks_it_read_in_the_list() {
        let mut app = sample_app();
        assert!(!app.envelopes()[0].flags.seen);
        app.start_loading_body(5);
        app.finish_body(5, Ok(message(5)));
        assert!(app.envelopes()[0].flags.seen);
    }

    #[test]
    fn a_stale_body_result_for_an_older_request_is_ignored() {
        let mut app = sample_app();
        app.start_loading_body(5);
        app.start_loading_body(4); // the user opened another message meanwhile

        app.finish_body(5, Ok(message(5)));
        assert!(matches!(app.body, BodyState::Loading { uid: 4 }));

        app.finish_body(4, Err("boom".to_string()));
        assert!(matches!(&app.body, BodyState::Error(e) if e == "boom"));
    }

    #[test]
    fn scroll_body_clamps_to_the_rendered_content_and_needs_an_open_message() {
        let mut app = sample_app();
        app.body_viewport.set(BodyViewport {
            height: 10,
            max_scroll: 25,
        });
        app.scroll_body(5);
        assert_eq!(app.body_scroll, 0, "nothing is open yet");

        app.start_loading_body(5);
        app.finish_body(5, Ok(message(5)));
        app.scroll_body(5);
        assert_eq!(app.body_scroll, 5);
        app.page_body(true);
        assert_eq!(
            app.body_scroll, 14,
            "a page is the pane height minus one line of overlap"
        );
        app.page_body(true);
        assert_eq!(app.body_scroll, 23);
        app.page_body(true);
        assert_eq!(app.body_scroll, 25, "clamped to the last screenful");
        app.scroll_body(-100);
        assert_eq!(app.body_scroll, 0);
    }

    #[test]
    fn opening_another_message_resets_the_scroll() {
        let mut app = sample_app();
        app.body_scroll = 12;
        app.start_loading_body(4);
        assert_eq!(app.body_scroll, 0);
    }

    #[test]
    fn removing_the_message_being_loaded_clears_the_reading_pane() {
        let mut app = sample_app();
        app.start_loading_body(3);
        app.remove_envelope(3);
        assert!(matches!(app.body, BodyState::Empty));
    }

    #[test]
    fn editing_the_search_refilters_and_a_cursor_move_keeps_the_selection() {
        let mut app = sample_app();
        app.start_search();
        app.selected = 3;
        app.edit_search(|s| s.insert('b'));
        assert_eq!(app.selected, 0, "a changed query jumps back to the first match");
        assert!(app.visible_indices().len() < 5);

        app.selected = 1;
        app.edit_search(TextInput::left);
        assert_eq!(app.selected, 1, "moving within the field doesn't change the query");
    }

    // -- search lifecycle -----------------------------------------------------

    #[test]
    fn start_search_opens_an_empty_editable_query_and_resets_selection() {
        let mut app = sample_app();
        app.selected = 3;
        app.start_search();

        assert!(app.search_editing);
        assert_eq!(app.search().unwrap().value, "");
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn clear_search_removes_the_filter_and_resets_selection() {
        let mut app = sample_app();
        app.set_search_query("invoice");
        app.search_editing = true;
        app.selected = 0;

        app.clear_search();

        assert!(app.search().is_none());
        assert!(!app.search_editing);
        assert_eq!(app.visible_indices().len(), 5);
    }

    // -- status message TTL ----------------------------------------------------

    #[test]
    fn expire_status_clears_a_message_past_its_ttl() {
        let mut app = sample_app();
        app.status_message = Some((
            "old".to_string(),
            Instant::now() - STATUS_MESSAGE_TTL - Duration::from_secs(1),
        ));

        let remaining = app.expire_status();

        assert!(remaining.is_none());
        assert!(app.status_message.is_none());
    }

    #[test]
    fn expire_status_leaves_a_fresh_message_in_place() {
        let mut app = sample_app();
        app.set_status("fresh");

        let remaining = app.expire_status();

        assert!(remaining.is_some());
        assert!(app.status_message.is_some());
    }

    // -- account switch reset --------------------------------------------------

    #[test]
    fn reset_for_account_switch_clears_everything_specific_to_the_previous_account() {
        let mut app = sample_app();
        app.selected = 3;
        app.set_search_query("x");
        app.has_more_older = false;
        app.loading_more = true;
        app.pending_g = true;
        app.known_senders = vec![Address {
            name: None,
            email: "x@example.com".to_string(),
        }];
        app.body = BodyState::Loaded(Box::new(Message {
            uid: 1,
            ..Message::default()
        }));
        app.outbox.pending = 2;

        app.reset_for_account_switch(9);

        assert_eq!(app.current_folder_id, 9);
        assert_eq!(app.outbox, OutboxCounts::default());
        assert!(app.envelopes().is_empty());
        assert_eq!(app.selected, 0);
        assert!(matches!(app.body, BodyState::Empty));
        assert!(matches!(app.list_state, ListState::Loading));
        assert!(app.search().is_none());
        assert!(app.has_more_older);
        assert!(!app.loading_more);
        assert!(!app.pending_g);
        assert!(app.known_senders.is_empty());
    }

    // -- ComposeState::id ---------------------------------------------------

    #[test]
    fn blank_compose_states_get_distinct_ids() {
        let a = ComposeState::blank();
        let b = ComposeState::blank();
        assert_ne!(a.id, b.id, "each compose session must be individually identifiable");
    }

    fn message(uid: u32) -> Message {
        Message {
            uid,
            subject: "Hi".to_string(),
            from: vec![Address {
                name: Some("Alice".to_string()),
                email: "alice@example.com".to_string(),
            }],
            message_id: Some("orig@example.com".to_string()),
            body_text: "hello".to_string(),
            attachments: vec![mail_core::MessageAttachment {
                filename: "a.pdf".to_string(),
                size_bytes: 10,
            }],
            ..Message::default()
        }
    }

    #[test]
    fn reply_compose_state_also_gets_a_fresh_id_and_draft_key() {
        let blank = ComposeState::blank();
        let reply = ComposeState::respond(&message(1), Respond::Reply, "me@example.com", 3);
        assert_ne!(blank.id, reply.id);
        assert_ne!(blank.draft_key, reply.draft_key);
    }

    #[test]
    fn a_reply_is_threaded_quoted_and_answers_its_message() {
        let reply = ComposeState::respond(&message(4), Respond::Reply, "me@example.com", 3);
        assert_eq!(reply.to.value, "Alice <alice@example.com>");
        assert_eq!(reply.subject.value, "Re: Hi");
        assert_eq!(reply.in_reply_to.as_deref(), Some("orig@example.com"));
        assert_eq!(reply.references, vec!["orig@example.com"]);
        assert_eq!(reply.answers, Some((3, 4)));
        assert_eq!(reply.body.lines.last().map(String::as_str), Some("> hello"));
        assert_eq!((reply.body.cursor_row, reply.body.cursor_col), (0, 0));
        assert!(reply.attachments.items.is_empty());
        let draft = reply.to_draft();
        assert_eq!(draft.in_reply_to.as_deref(), Some("orig@example.com"));
    }

    #[test]
    fn a_forward_carries_the_attachments_and_starts_in_to() {
        let fwd = ComposeState::respond(&message(4), Respond::Forward, "me@example.com", 3);
        assert_eq!(fwd.focus, ComposeField::To);
        assert_eq!(fwd.subject.value, "Fwd: Hi");
        assert!(fwd.to.value.is_empty());
        assert_eq!(fwd.in_reply_to, None);
        assert_eq!(
            fwd.attachments.items[0].source,
            AttachmentSource::Message {
                folder_id: 3,
                uid: 4,
                index: 0
            }
        );
    }

    // -- drafts -------------------------------------------------------------

    #[test]
    fn autosave_waits_for_a_change_and_the_interval() {
        let start = Instant::now();
        let mut compose = ComposeState::blank();
        assert!(
            compose.autosave_due(start).is_none(),
            "nothing to save in a blank compose"
        );

        compose.subject = TextInput::with_value("Hi");
        let first = compose.autosave_due(start).expect("a change is saved");
        assert_eq!(first.draft.subject, "Hi");
        assert_eq!(first.key, compose.draft_key);
        assert!(
            compose.autosave_due(start + AUTOSAVE_INTERVAL).is_none(),
            "unchanged since"
        );

        compose.subject = TextInput::with_value("Hi there");
        assert!(
            compose.autosave_due(start + Duration::from_secs(1)).is_none(),
            "too soon after the last save"
        );
        assert!(compose.autosave_due(start + AUTOSAVE_INTERVAL).is_some());
    }

    #[test]
    fn a_failed_save_is_retried() {
        let now = Instant::now();
        let mut compose = ComposeState::blank();
        compose.subject = TextInput::with_value("Hi");
        compose.take_save(now);
        assert!(!compose.has_unsaved_changes());
        compose.save_failed();
        assert!(compose.has_unsaved_changes());
    }

    #[test]
    fn a_resumed_draft_comes_back_whole_and_unchanged() {
        let mut original = ComposeState::respond(&message(4), Respond::ReplyAll, "me@example.com", 3);
        original.body.lines[0] = "Sounds good".to_string();
        original.attachments.items.push(AttachmentEntry {
            source: AttachmentSource::File(std::path::PathBuf::from("/tmp/x")),
            filename: "x".to_string(),
            size_bytes: 1,
        });
        let saved = original.take_save(Instant::now());

        let resumed = ComposeState::from_saved(saved.clone());
        assert_eq!(resumed.to_saved(), saved);
        assert_eq!(resumed.draft_key, original.draft_key);
        assert!(!resumed.has_unsaved_changes());
        assert_eq!(resumed.focus, ComposeField::Body);
        assert_eq!(resumed.vim.mode, VimMode::Normal);
    }

    // -- folders and flags ----------------------------------------------------

    fn folder(id: i64, name: &str, selectable: bool, unseen: u32) -> Folder {
        Folder {
            id,
            account_id: 1,
            name: name.to_string(),
            delimiter: Some("/".to_string()),
            role: None,
            selectable,
            unseen: Some(unseen),
            total: Some(10),
        }
    }

    #[test]
    fn folder_navigation_skips_containers_and_wraps() {
        let mut app = sample_app();
        app.set_folders(
            0,
            vec![
                folder(1, "INBOX", true, 0),
                folder(2, "[Gmail]", false, 0),
                folder(3, "[Gmail]/Sent", true, 0),
            ],
        );
        app.current_folder_id = 1;
        assert_eq!(app.neighbor_folder(true), Some(3));
        assert_eq!(app.neighbor_folder(false), Some(3));
        app.show_folder(3);
        assert_eq!(app.neighbor_folder(true), Some(1));
        assert_eq!(app.current_folder().unwrap().name, "[Gmail]/Sent");
        assert!(app.envelopes().is_empty(), "switching folders clears the old list");
    }

    #[test]
    fn toggling_read_keeps_the_unread_count_in_step() {
        let mut app = sample_app();
        app.set_folders(0, vec![folder(1, "INBOX", true, 5)]);
        app.current_folder_id = 1;

        assert!(app.set_envelope_flag(4, FlagKind::Seen, true));
        assert!(
            !app.set_envelope_flag(4, FlagKind::Seen, true),
            "no change, no count change"
        );
        assert_eq!(app.current_folder().unwrap().unseen, Some(4));
        assert!(app.set_envelope_flag(4, FlagKind::Seen, false));
        assert_eq!(app.current_folder().unwrap().unseen, Some(5));

        assert!(app.set_envelope_flag(4, FlagKind::Flagged, true));
        assert!(app.envelopes()[1].flags.flagged);
        assert_eq!(app.current_folder().unwrap().unseen, Some(5));
    }

    // -- last_recipient_separator ------------------------------------------

    #[test]
    fn last_recipient_separator_finds_the_last_plain_comma() {
        assert_eq!(last_recipient_separator("a@x.com, b@x.com, c@x.com"), Some(16));
    }

    #[test]
    fn last_recipient_separator_ignores_a_comma_inside_quotes() {
        let s = "\"Doe, Jane\" <jane@example.com>, ali";
        // The separator must be the comma after the closing quote's
        // address, not the one inside "Doe, Jane".
        assert_eq!(&s[last_recipient_separator(s).unwrap()..], ", ali");
    }

    #[test]
    fn last_recipient_separator_treats_an_unterminated_quote_as_still_open() {
        // Still typing the quoted name — the comma inside it must not
        // count as a separator even though the closing quote hasn't
        // been typed yet.
        assert_eq!(last_recipient_separator("\"Doe, Jane"), None);
    }

    #[test]
    fn last_recipient_separator_returns_none_with_no_comma_at_all() {
        assert_eq!(last_recipient_separator("alice@example.com"), None);
    }

    // -- compose recipient suggestions ------------------------------------

    fn senders() -> Vec<Address> {
        vec![
            Address {
                name: Some("Alice Doe".to_string()),
                email: "alice@example.com".to_string(),
            },
            Address {
                name: Some("Bob Smith".to_string()),
                email: "bob@example.com".to_string(),
            },
        ]
    }

    #[test]
    fn refresh_suggestions_matches_against_the_fragment_after_the_last_comma() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::To;
        compose.to = TextInput::with_value("bob@example.com, ali".to_string());

        compose.refresh_suggestions(&senders());

        assert_eq!(compose.suggestions.items.len(), 1);
        assert_eq!(compose.suggestions.items[0].email, "alice@example.com");
    }

    #[test]
    fn refresh_suggestions_is_empty_when_the_fragment_is_blank() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::To;
        compose.to = TextInput::with_value("alice@example.com, ".to_string());

        compose.refresh_suggestions(&senders());

        assert!(compose.suggestions.items.is_empty());
    }

    #[test]
    fn refresh_suggestions_is_always_empty_outside_recipient_fields() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::Subject;
        compose.subject = TextInput::with_value("alice".to_string());

        compose.refresh_suggestions(&senders());

        assert!(compose.suggestions.items.is_empty());
    }

    #[test]
    fn accept_suggestion_replaces_the_trailing_fragment_and_appends_a_separator() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::To;
        compose.to = TextInput::with_value("ali".to_string());
        compose.refresh_suggestions(&senders());
        assert_eq!(compose.suggestions.items.len(), 1);

        compose.accept_suggestion();

        assert_eq!(compose.to.value, "Alice Doe <alice@example.com>, ");
        assert_eq!(compose.to.cursor, compose.to.value.chars().count());
        assert!(compose.suggestions.items.is_empty());
    }

    #[test]
    fn accept_suggestion_preserves_earlier_recipients_before_the_last_comma() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::To;
        compose.to = TextInput::with_value("bob@example.com, ali".to_string());
        compose.refresh_suggestions(&senders());

        compose.accept_suggestion();

        assert_eq!(compose.to.value, "bob@example.com, Alice Doe <alice@example.com>, ");
    }

    #[test]
    fn refresh_suggestions_ignores_a_comma_inside_a_quoted_display_name() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::To;
        // A "Last, First" quoted display name — the comma inside the
        // quotes must not be mistaken for the recipient separator.
        compose.to = TextInput::with_value("\"Doe, Jane\" <jane@example.com>, ali".to_string());

        compose.refresh_suggestions(&senders());

        assert_eq!(compose.suggestions.items.len(), 1);
        assert_eq!(compose.suggestions.items[0].email, "alice@example.com");
    }

    #[test]
    fn accept_suggestion_preserves_a_quoted_recipient_containing_a_comma() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::To;
        compose.to = TextInput::with_value("\"Doe, Jane\" <jane@example.com>, ali".to_string());
        compose.refresh_suggestions(&senders());

        compose.accept_suggestion();

        assert_eq!(
            compose.to.value,
            "\"Doe, Jane\" <jane@example.com>, Alice Doe <alice@example.com>, "
        );
    }

    #[test]
    fn an_accepted_name_with_a_comma_is_quoted() {
        let mut compose = ComposeState::blank();
        compose.to = TextInput::with_value("doe".to_string());
        compose.suggestions.items = vec![Address {
            name: Some("Doe, Jane".to_string()),
            email: "jane@example.com".to_string(),
        }];
        compose.accept_suggestion();
        assert_eq!(compose.to.value, "\"Doe, Jane\" <jane@example.com>, ");
    }

    #[test]
    fn accept_suggestion_is_a_no_op_when_there_is_nothing_selected() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::To;
        compose.to = TextInput::with_value("zzz_no_match".to_string());
        compose.refresh_suggestions(&senders());
        assert!(compose.suggestions.items.is_empty());

        compose.accept_suggestion();

        assert_eq!(compose.to.value, "zzz_no_match");
    }

    // -- Selectable<T> ---------------------------------------------------

    #[test]
    fn selectable_move_up_and_down_clamp_at_the_list_bounds() {
        let mut s = Selectable {
            items: vec!['a', 'b', 'c'],
            selected: 0,
        };
        s.move_up();
        assert_eq!(s.selected, 0, "move_up must not go below 0");

        s.move_down();
        s.move_down();
        s.move_down();
        assert_eq!(s.selected, 2, "move_down must clamp at the last item");
    }

    #[test]
    fn selectable_move_up_and_down_on_an_empty_list_do_nothing() {
        let mut s: Selectable<char> = Selectable::default();
        s.move_down();
        assert_eq!(s.selected, 0);
        s.move_up();
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn selectable_remove_selected_clamps_onto_the_new_last_item() {
        let mut s = Selectable {
            items: vec!['a', 'b', 'c'],
            selected: 2,
        };
        s.remove_selected();
        assert_eq!(s.items, vec!['a', 'b']);
        assert_eq!(s.selected, 1, "selection must move back onto the new last item");
    }

    #[test]
    fn selectable_remove_selected_on_an_empty_list_is_a_no_op() {
        let mut s: Selectable<char> = Selectable::default();
        s.remove_selected();
        assert!(s.items.is_empty());
    }

    #[test]
    fn selectable_selected_item_reflects_the_current_index() {
        let s = Selectable {
            items: vec!['a', 'b', 'c'],
            selected: 1,
        };
        assert_eq!(s.selected_item(), Some(&'b'));

        let empty: Selectable<char> = Selectable::default();
        assert_eq!(empty.selected_item(), None);
    }

    #[test]
    fn selectable_clear_empties_the_list_and_resets_the_selection() {
        let mut s = Selectable {
            items: vec!['a', 'b'],
            selected: 1,
        };
        s.clear();
        assert!(s.items.is_empty());
        assert_eq!(s.selected, 0);
    }

    // -- compose spellcheck per-line state --------------------------------

    /// A one-entry misspelling map, as `set_line_misspellings`/
    /// `set_all_misspellings` take — `word` mapped to `suggestions`.
    fn misspelling(word: &str, suggestions: &[&str]) -> HashMap<String, Vec<String>> {
        HashMap::from([(word.to_string(), suggestions.iter().map(|s| s.to_string()).collect())])
    }

    #[test]
    fn set_line_misspellings_patches_only_the_targeted_line() {
        let mut compose = ComposeState::blank();
        compose.set_line_misspellings(0, misspelling("wrold", &["world"]));
        compose.set_line_misspellings(1, misspelling("tset", &["test"]));

        assert_eq!(compose.misspelled_by_line[0], misspelling("wrold", &["world"]));
        assert_eq!(compose.misspelled_by_line[1], misspelling("tset", &["test"]));
    }

    #[test]
    fn set_line_misspellings_grows_the_vec_to_fit_a_later_line() {
        let mut compose = ComposeState::blank();
        assert!(compose.misspelled_by_line.is_empty());

        compose.set_line_misspellings(2, misspelling("oops", &[]));

        assert_eq!(
            compose.misspelled_by_line.len(),
            3,
            "earlier lines must be backfilled empty"
        );
        assert!(compose.misspelled_by_line[0].is_empty());
        assert!(compose.misspelled_by_line[1].is_empty());
        assert_eq!(compose.misspelled_by_line[2], misspelling("oops", &[]));
    }

    #[test]
    fn set_all_misspellings_replaces_the_whole_vec_wholesale() {
        let mut compose = ComposeState::blank();
        compose.set_line_misspellings(0, misspelling("stale", &[]));

        compose.set_all_misspellings(vec![HashMap::new(), misspelling("fresh", &["fresher"])]);

        assert_eq!(compose.misspelled_by_line.len(), 2);
        assert!(
            compose.misspelled_by_line[0].is_empty(),
            "the stale single-line entry must be gone"
        );
        assert_eq!(compose.misspelled_by_line[1], misspelling("fresh", &["fresher"]));
    }

    // -- misspelled_word_at_cursor ------------------------------------------

    #[test]
    fn misspelled_word_at_cursor_finds_suggestions_for_the_flagged_word_the_cursor_is_on() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::Body;
        for c in "hello wrold".chars() {
            compose.body.insert(c);
        }
        compose.set_line_misspellings(0, misspelling("wrold", &["world", "wold"]));

        let (word, suggestions) = compose.misspelled_word_at_cursor().unwrap();
        assert_eq!(word, "wrold");
        assert_eq!(suggestions, ["world".to_string(), "wold".to_string()]);
    }

    #[test]
    fn misspelled_word_at_cursor_is_none_when_the_cursor_is_on_a_correctly_spelled_word() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::Body;
        for c in "hello world".chars() {
            compose.body.insert(c);
        }
        compose.set_line_misspellings(0, misspelling("wrold", &["world"]));

        assert_eq!(compose.misspelled_word_at_cursor(), None);
    }

    #[test]
    fn misspelled_word_at_cursor_is_none_outside_the_body_field() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::Subject;
        compose.body.lines = vec!["wrold".to_string()];
        compose.body.cursor_col = 5;
        compose.set_line_misspellings(0, misspelling("wrold", &["world"]));

        assert_eq!(
            compose.misspelled_word_at_cursor(),
            None,
            "a non-Body focus means a stale cursor position"
        );
    }

    // -- body_change ----------------------------------------------------------

    fn lines(text: &str) -> Vec<String> {
        text.split('\n').map(str::to_string).collect()
    }

    #[test]
    fn body_change_reports_no_change_one_line_or_a_full_recheck() {
        assert_eq!(body_change(&lines("a\nb"), &lines("a\nb")), BodyChange::None);
        assert_eq!(body_change(&lines("a\nb"), &lines("a\nbx")), BodyChange::Line(1));
        assert_eq!(
            body_change(&lines("a\nb"), &lines("ax\nbx")),
            BodyChange::Full,
            "two lines changed"
        );
        assert_eq!(
            body_change(&lines("a\nb"), &lines("a")),
            BodyChange::Full,
            "line count changed"
        );
        assert_eq!(body_change(&lines("a"), &lines("a\n")), BodyChange::Full);
    }

    // -- compose + vim integration -------------------------------------------

    use crate::vim::Mode as VimMode;

    #[test]
    fn tabbing_into_the_body_starts_insert_mode() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::Attachments;
        compose.next_field();
        assert_eq!(compose.focus, ComposeField::Body);
        assert_eq!(compose.vim.mode, VimMode::Insert);

        let mut compose = ComposeState::blank();
        compose.prev_field(); // To -> Body
        assert_eq!(compose.vim.mode, VimMode::Insert);
    }

    #[test]
    fn a_reply_opens_in_insert_mode_in_the_body() {
        let compose = ComposeState::respond(&message(1), Respond::Reply, "me@example.com", 1);
        assert_eq!(compose.focus, ComposeField::Body);
        assert_eq!(compose.vim.mode, VimMode::Insert);
    }

    #[test]
    fn leaving_the_body_resets_the_editor_to_normal_mode() {
        let mut compose = ComposeState::blank();
        compose.prev_field(); // into the body
        compose.next_field(); // back out to To
        assert_eq!(compose.focus, ComposeField::To);
        assert_eq!(compose.vim.mode, VimMode::Normal);
    }

    #[test]
    fn focus_body_normal_lands_in_normal_mode_with_the_cursor_on_a_char() {
        let mut compose = ComposeState::blank();
        compose.body.lines = vec!["abc".to_string()];
        compose.body.cursor_col = 3;
        compose.focus_body_normal();
        assert_eq!(compose.focus, ComposeField::Body);
        assert_eq!(compose.vim.mode, VimMode::Normal);
        assert_eq!(compose.body.cursor_col, 2);
    }

    #[test]
    fn is_blank_considers_every_field_and_attachments() {
        assert!(ComposeState::blank().is_blank());

        let mut compose = ComposeState::blank();
        compose.body.lines = vec!["  ".to_string(), String::new()];
        assert!(compose.is_blank(), "whitespace-only body still counts as blank");

        for set in [
            |c: &mut ComposeState| c.to = TextInput::with_value("a@example.com"),
            |c: &mut ComposeState| c.subject = TextInput::with_value("Hi"),
            |c: &mut ComposeState| c.body.lines = vec!["text".to_string()],
            |c: &mut ComposeState| {
                c.attachments.items.push(AttachmentEntry {
                    source: crate::attach::AttachmentSource::File(std::path::PathBuf::from("/tmp/x")),
                    filename: "x".to_string(),
                    size_bytes: 1,
                })
            },
        ] {
            let mut compose = ComposeState::blank();
            set(&mut compose);
            assert!(!compose.is_blank());
        }
    }
}
