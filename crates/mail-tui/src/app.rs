use std::time::{Duration, Instant};

use mail_core::{Address, Draft, Envelope, Message};

use crate::attach::AttachmentEntry;
use crate::editable::{TextArea, TextInput};
use crate::filebrowser::FileBrowser;

/// How long a status message (e.g. "Sent.") stays in the status bar
/// before it's cleared automatically, so the key hints underneath it
/// become visible again without needing another keypress.
pub const STATUS_MESSAGE_TTL: Duration = Duration::from_secs(5);

/// How many rows a vim-style `Ctrl-d`/`Ctrl-u` half-page jump moves.
pub const PAGE_JUMP: usize = 10;

/// How many fuzzy-matched sender suggestions to show at once under a
/// recipient field in compose.
pub const MAX_RECIPIENT_SUGGESTIONS: usize = 8;

/// What a currently-open `FileBrowser` is for — decides what happens
/// when the user picks a file or confirms a save location.
pub enum BrowserPurpose {
    AttachToCompose,
    /// The original filename lives in the browser's own editable
    /// `filename` field (prefilled from it); only the bytes need
    /// carrying separately.
    SaveAttachment { bytes: Vec<u8> },
}

pub enum BodyState {
    Empty,
    Loading,
    Loaded(Message),
    Error(String),
}

pub enum ListState {
    Loading,
    Loaded,
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ComposeField {
    To,
    Cc,
    Bcc,
    Subject,
    Attachments,
    Body,
}

pub struct ComposeState {
    pub to: TextInput,
    pub cc: TextInput,
    pub bcc: TextInput,
    pub subject: TextInput,
    pub body: TextArea,
    pub focus: ComposeField,
    pub attachments: Vec<AttachmentEntry>,
    pub attachment_selected: usize,
    pub sending: bool,
    pub error: Option<String>,
    /// Fuzzy-matched sender suggestions for whatever's typed after the
    /// last comma in the currently focused recipient field (To/Cc/Bcc);
    /// empty hides the dropdown. Recomputed by `refresh_suggestions`.
    pub suggestions: Vec<Address>,
    pub suggestion_selected: usize,
}

impl ComposeState {
    pub fn blank() -> Self {
        Self {
            to: TextInput::default(),
            cc: TextInput::default(),
            bcc: TextInput::default(),
            subject: TextInput::default(),
            body: TextArea::default(),
            focus: ComposeField::To,
            attachments: Vec::new(),
            attachment_selected: 0,
            sending: false,
            error: None,
            suggestions: Vec::new(),
            suggestion_selected: 0,
        }
    }

    /// A reply to `source`, prefilled from its envelope (from/subject) —
    /// doesn't require the body to have been fetched.
    pub fn reply(source: &Envelope) -> Self {
        let to = source
            .from
            .first()
            .map(|a| a.email.clone())
            .unwrap_or_default();
        let subject = if source.subject.to_lowercase().starts_with("re:") {
            source.subject.clone()
        } else {
            format!("Re: {}", source.subject)
        };
        Self {
            to: TextInput::with_value(to),
            subject: TextInput::with_value(subject),
            focus: ComposeField::Body,
            ..Self::blank()
        }
    }

    pub fn next_field(&mut self) {
        self.focus = match self.focus {
            ComposeField::To => ComposeField::Cc,
            ComposeField::Cc => ComposeField::Bcc,
            ComposeField::Bcc => ComposeField::Subject,
            ComposeField::Subject => ComposeField::Attachments,
            ComposeField::Attachments => ComposeField::Body,
            ComposeField::Body => ComposeField::To,
        };
    }

    pub fn prev_field(&mut self) {
        self.focus = match self.focus {
            ComposeField::To => ComposeField::Body,
            ComposeField::Cc => ComposeField::To,
            ComposeField::Bcc => ComposeField::Cc,
            ComposeField::Subject => ComposeField::Bcc,
            ComposeField::Attachments => ComposeField::Subject,
            ComposeField::Body => ComposeField::Attachments,
        };
    }

    pub fn to_draft(&self) -> Draft {
        Draft {
            to: self.to.value.clone(),
            cc: self.cc.value.clone(),
            bcc: self.bcc.value.clone(),
            subject: self.subject.value.clone(),
            body: self.body.text(),
        }
    }

    pub fn attachments_total_bytes(&self) -> u64 {
        self.attachments.iter().map(|a| a.size_bytes).sum()
    }

    pub fn remove_selected_attachment(&mut self) {
        if self.attachments.is_empty() {
            return;
        }
        self.attachments.remove(self.attachment_selected);
        if self.attachment_selected >= self.attachments.len() {
            self.attachment_selected = self.attachments.len().saturating_sub(1);
        }
    }

    /// Recomputes `suggestions` against whatever's typed after the last
    /// comma in the currently focused field — call after every edit or
    /// focus change. Clears immediately outside To/Cc/Bcc, or when that
    /// fragment is empty (nothing to suggest against yet).
    pub fn refresh_suggestions(&mut self, known_senders: &[Address]) {
        self.suggestion_selected = 0;
        let field_value = match self.focus {
            ComposeField::To => &self.to.value,
            ComposeField::Cc => &self.cc.value,
            ComposeField::Bcc => &self.bcc.value,
            ComposeField::Subject | ComposeField::Attachments | ComposeField::Body => {
                self.suggestions.clear();
                return;
            }
        };
        let query = field_value.rsplit(',').next().unwrap_or("").trim();
        self.suggestions = if query.is_empty() {
            Vec::new()
        } else {
            crate::fuzzy::best_matches(query, known_senders, MAX_RECIPIENT_SUGGESTIONS)
        };
    }

    /// Accepts the highlighted suggestion into the focused recipient
    /// field, replacing whatever was typed after the last comma and
    /// leaving a trailing ", " ready for the next address. A no-op
    /// outside To/Cc/Bcc, or when there's nothing selected to accept.
    pub fn accept_suggestion(&mut self) {
        let Some(chosen) = self.suggestions.get(self.suggestion_selected).cloned() else {
            return;
        };
        let field = match self.focus {
            ComposeField::To => &mut self.to,
            ComposeField::Cc => &mut self.cc,
            ComposeField::Bcc => &mut self.bcc,
            ComposeField::Subject | ComposeField::Attachments | ComposeField::Body => return,
        };
        let prefix = match field.value.rfind(',') {
            Some(idx) => format!("{} ", field.value[..=idx].trim_end()),
            None => String::new(),
        };
        field.value = format!("{prefix}{chosen}, ");
        field.cursor = field.value.chars().count();
        self.suggestions.clear();
        self.suggestion_selected = 0;
    }
}

pub struct App {
    /// Every configured account's email, in the fixed order they were
    /// loaded at startup — index-matched with the `Accounts` list main.rs
    /// owns. Doesn't change after startup; only `current_account` does.
    pub account_emails: Vec<String>,
    pub current_account: usize,
    pub list_state: ListState,
    pub envelopes: Vec<Envelope>,
    pub selected: usize,
    pub body: BodyState,
    /// Which of the open message's attachments is highlighted for
    /// download — reset to 0 whenever a new message body loads.
    pub selected_attachment: usize,
    pub compose: Option<ComposeState>,
    /// The address book fuzzy-matched against while typing a recipient in
    /// compose: every sender the current account has received mail from,
    /// minus no-reply senders. Loaded asynchronously (see `main.rs`'s
    /// `spawn_known_senders`), so it may briefly be empty right after
    /// startup or an account switch.
    pub known_senders: Vec<Address>,
    pub status_message: Option<(String, Instant)>,
    /// `Some` whenever a filter is showing (being edited or confirmed).
    /// An empty query is equivalent to no filter.
    pub search: Option<TextInput>,
    pub search_editing: bool,
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
            account_emails,
            current_account: 0,
            list_state: ListState::Loading,
            envelopes: Vec::new(),
            selected: 0,
            body: BodyState::Empty,
            selected_attachment: 0,
            compose: None,
            known_senders: Vec::new(),
            status_message: None,
            search: None,
            search_editing: false,
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
    pub fn reset_for_account_switch(&mut self) {
        self.envelopes.clear();
        self.selected = 0;
        self.selected_attachment = 0;
        self.body = BodyState::Empty;
        self.list_state = ListState::Loading;
        self.clear_search();
        self.has_more_older = true;
        self.loading_more = false;
        self.pending_g = false;
        self.known_senders.clear();
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

    /// Indices into `envelopes` that match the active search query, in
    /// display order — all of them when there's no query. Recomputed on
    /// demand rather than cached: the envelope list is small (bounded by
    /// `fetch_limit`), so this stays cheap enough to call every frame.
    pub fn visible_indices(&self) -> Vec<usize> {
        match &self.search {
            Some(q) if !q.value.trim().is_empty() => {
                let needle = q.value.to_lowercase();
                self.envelopes
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| envelope_matches(e, &needle))
                    .map(|(i, _)| i)
                    .collect()
            }
            _ => (0..self.envelopes.len()).collect(),
        }
    }

    pub fn select_next(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        self.selected = (self.selected + 1).min(visible.len() - 1);
    }

    /// Whether the selection is on the last visible row — the trigger
    /// point for "scroll to the bottom to load more older mail". Only
    /// meaningful (and only checked by the caller) with no search filter
    /// active, since "end of the filtered results" isn't the same thing
    /// as "end of what's cached".
    pub fn is_at_end_of_list(&self) -> bool {
        let visible_len = self.visible_indices().len();
        visible_len > 0 && self.selected + 1 >= visible_len
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
        let visible_len = self.visible_indices().len();
        self.selected = visible_len.saturating_sub(1);
    }

    /// Vim-style `Ctrl-d` — jump `PAGE_JUMP` rows down, clamped to the
    /// last visible row.
    pub fn select_page_down(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        self.selected = (self.selected + PAGE_JUMP).min(visible.len() - 1);
    }

    /// Vim-style `Ctrl-u` — jump `PAGE_JUMP` rows up, clamped to the top.
    pub fn select_page_up(&mut self) {
        self.selected = self.selected.saturating_sub(PAGE_JUMP);
    }

    pub fn selected_uid(&self) -> Option<u32> {
        self.selected_envelope().map(|e| e.uid)
    }

    pub fn selected_envelope(&self) -> Option<&Envelope> {
        let visible = self.visible_indices();
        visible.get(self.selected).and_then(|&i| self.envelopes.get(i))
    }

    pub fn start_search(&mut self) {
        self.search = Some(TextInput::default());
        self.search_editing = true;
        self.selected = 0;
    }

    /// Drops a deleted message from view and keeps `selected` in bounds,
    /// clearing the reading pane too if that's the message it was showing.
    pub fn remove_envelope(&mut self, uid: u32) {
        self.envelopes.retain(|e| e.uid != uid);

        let visible_len = self.visible_indices().len();
        if self.selected >= visible_len {
            self.selected = visible_len.saturating_sub(1);
        }

        if let BodyState::Loaded(message) = &self.body {
            if message.uid == uid {
                self.body = BodyState::Empty;
            }
        }
    }

    pub fn clear_search(&mut self) {
        self.search = None;
        self.search_editing = false;
        self.selected = 0;
    }
}

fn envelope_matches(e: &Envelope, needle: &str) -> bool {
    e.subject.to_lowercase().contains(needle)
        || e.from.iter().any(|a| {
            a.email.to_lowercase().contains(needle)
                || a.name.as_deref().unwrap_or("").to_lowercase().contains(needle)
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
            from: vec![Address { name: Some(from_name.to_string()), email: from_email.to_string() }],
            to: Vec::new(),
            date: None,
            flags: Flags::default(),
            has_attachments: false,
        }
    }

    fn app_with(envelopes: Vec<Envelope>) -> App {
        let mut app = App::new(vec!["me@example.com".to_string()]);
        app.envelopes = envelopes;
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
        app.search = Some(TextInput::with_value("invoice".to_string()));
        assert_eq!(app.visible_indices(), vec![2]);
    }

    #[test]
    fn visible_indices_filters_by_sender_name_or_email() {
        let mut app = sample_app();
        app.search = Some(TextInput::with_value("alice".to_string()));
        assert_eq!(app.visible_indices(), vec![1]);

        app.search = Some(TextInput::with_value("bob@example.com".to_string()));
        assert_eq!(app.visible_indices(), vec![3]);
    }

    #[test]
    fn visible_indices_treats_blank_or_whitespace_query_as_no_filter() {
        let mut app = sample_app();
        app.search = Some(TextInput::with_value("   ".to_string()));
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
        assert_eq!(app.selected, 4, "only 5 rows exist, so a 10-row jump clamps to the last one");

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
        app.search = Some(TextInput::with_value("a".to_string())); // matches several
        let visible = app.visible_indices();
        assert!(!visible.is_empty());
        assert_eq!(app.selected_uid(), Some(app.envelopes[visible[0]].uid));
    }

    #[test]
    fn selected_uid_is_none_when_filter_matches_nothing() {
        let mut app = sample_app();
        app.search = Some(TextInput::with_value("zzz_no_match".to_string()));
        assert_eq!(app.selected_uid(), None);
    }

    // -- remove_envelope ------------------------------------------------------

    #[test]
    fn remove_envelope_drops_the_row_and_keeps_selection_in_bounds() {
        let mut app = sample_app();
        app.selected = 4; // last row (uid 1)
        app.remove_envelope(1);

        assert_eq!(app.envelopes.len(), 4);
        assert!(app.envelopes.iter().all(|e| e.uid != 1));
        assert_eq!(app.selected, 3, "selection must move back onto the new last row");
    }

    #[test]
    fn remove_envelope_clears_the_reading_pane_if_it_was_showing_that_message() {
        let mut app = sample_app();
        app.body = BodyState::Loaded(Message {
            uid: 3,
            subject: "Invoice #42".to_string(),
            from: Vec::new(),
            to: Vec::new(),
            date: None,
            body_text: String::new(),
            attachments: Vec::new(),
        });

        app.remove_envelope(3);

        assert!(matches!(app.body, BodyState::Empty));
    }

    #[test]
    fn remove_envelope_leaves_the_reading_pane_alone_for_a_different_message() {
        let mut app = sample_app();
        app.body = BodyState::Loaded(Message {
            uid: 3,
            subject: "Invoice #42".to_string(),
            from: Vec::new(),
            to: Vec::new(),
            date: None,
            body_text: String::new(),
            attachments: Vec::new(),
        });

        app.remove_envelope(1);

        assert!(matches!(app.body, BodyState::Loaded(_)));
    }

    // -- search lifecycle -----------------------------------------------------

    #[test]
    fn start_search_opens_an_empty_editable_query_and_resets_selection() {
        let mut app = sample_app();
        app.selected = 3;
        app.start_search();

        assert!(app.search_editing);
        assert_eq!(app.search.as_ref().unwrap().value, "");
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn clear_search_removes_the_filter_and_resets_selection() {
        let mut app = sample_app();
        app.search = Some(TextInput::with_value("invoice".to_string()));
        app.search_editing = true;
        app.selected = 0;

        app.clear_search();

        assert!(app.search.is_none());
        assert!(!app.search_editing);
        assert_eq!(app.visible_indices().len(), 5);
    }

    // -- status message TTL ----------------------------------------------------

    #[test]
    fn expire_status_clears_a_message_past_its_ttl() {
        let mut app = sample_app();
        app.status_message = Some(("old".to_string(), Instant::now() - STATUS_MESSAGE_TTL - Duration::from_secs(1)));

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
        app.search = Some(TextInput::with_value("x".to_string()));
        app.has_more_older = false;
        app.loading_more = true;
        app.pending_g = true;
        app.known_senders = vec![Address { name: None, email: "x@example.com".to_string() }];
        app.body = BodyState::Loaded(Message {
            uid: 1,
            subject: String::new(),
            from: Vec::new(),
            to: Vec::new(),
            date: None,
            body_text: String::new(),
            attachments: Vec::new(),
        });

        app.reset_for_account_switch();

        assert!(app.envelopes.is_empty());
        assert_eq!(app.selected, 0);
        assert!(matches!(app.body, BodyState::Empty));
        assert!(matches!(app.list_state, ListState::Loading));
        assert!(app.search.is_none());
        assert!(app.has_more_older);
        assert!(!app.loading_more);
        assert!(!app.pending_g);
        assert!(app.known_senders.is_empty());
    }

    // -- compose recipient suggestions ------------------------------------

    fn senders() -> Vec<Address> {
        vec![
            Address { name: Some("Alice Doe".to_string()), email: "alice@example.com".to_string() },
            Address { name: Some("Bob Smith".to_string()), email: "bob@example.com".to_string() },
        ]
    }

    #[test]
    fn refresh_suggestions_matches_against_the_fragment_after_the_last_comma() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::To;
        compose.to = TextInput::with_value("bob@example.com, ali".to_string());

        compose.refresh_suggestions(&senders());

        assert_eq!(compose.suggestions.len(), 1);
        assert_eq!(compose.suggestions[0].email, "alice@example.com");
    }

    #[test]
    fn refresh_suggestions_is_empty_when_the_fragment_is_blank() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::To;
        compose.to = TextInput::with_value("alice@example.com, ".to_string());

        compose.refresh_suggestions(&senders());

        assert!(compose.suggestions.is_empty());
    }

    #[test]
    fn refresh_suggestions_is_always_empty_outside_recipient_fields() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::Subject;
        compose.subject = TextInput::with_value("alice".to_string());

        compose.refresh_suggestions(&senders());

        assert!(compose.suggestions.is_empty());
    }

    #[test]
    fn accept_suggestion_replaces_the_trailing_fragment_and_appends_a_separator() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::To;
        compose.to = TextInput::with_value("ali".to_string());
        compose.refresh_suggestions(&senders());
        assert_eq!(compose.suggestions.len(), 1);

        compose.accept_suggestion();

        assert_eq!(compose.to.value, "Alice Doe <alice@example.com>, ");
        assert_eq!(compose.to.cursor, compose.to.value.chars().count());
        assert!(compose.suggestions.is_empty());
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
    fn accept_suggestion_is_a_no_op_when_there_is_nothing_selected() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::To;
        compose.to = TextInput::with_value("zzz_no_match".to_string());
        compose.refresh_suggestions(&senders());
        assert!(compose.suggestions.is_empty());

        compose.accept_suggestion();

        assert_eq!(compose.to.value, "zzz_no_match");
    }
}
