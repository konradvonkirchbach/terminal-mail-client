use mail_core::{Draft, Envelope, Message};

use crate::attach::AttachmentEntry;
use crate::editable::{TextArea, TextInput};

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
    /// `Some` while the "attach a file" modal is open. The `String` is a
    /// validation error from the last attempt, shown inline until the
    /// user edits the path again.
    pub attach_prompt: Option<(TextInput, Option<String>)>,
    pub sending: bool,
    pub error: Option<String>,
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
            attach_prompt: None,
            sending: false,
            error: None,
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

    pub fn remove_selected_attachment(&mut self) {
        if self.attachments.is_empty() {
            return;
        }
        self.attachments.remove(self.attachment_selected);
        if self.attachment_selected >= self.attachments.len() {
            self.attachment_selected = self.attachments.len().saturating_sub(1);
        }
    }
}

pub struct App {
    pub account_email: String,
    pub list_state: ListState,
    pub envelopes: Vec<Envelope>,
    pub selected: usize,
    pub body: BodyState,
    pub compose: Option<ComposeState>,
    pub status_message: Option<String>,
    /// `Some` whenever a filter is showing (being edited or confirmed).
    /// An empty query is equivalent to no filter.
    pub search: Option<TextInput>,
    pub search_editing: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new(account_email: String) -> Self {
        Self {
            account_email,
            list_state: ListState::Loading,
            envelopes: Vec::new(),
            selected: 0,
            body: BodyState::Empty,
            compose: None,
            status_message: None,
            search: None,
            search_editing: false,
            should_quit: false,
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

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
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
