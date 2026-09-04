use mail_core::{Draft, Envelope, Message};

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
    Body,
}

pub struct ComposeState {
    pub to: TextInput,
    pub cc: TextInput,
    pub bcc: TextInput,
    pub subject: TextInput,
    pub body: TextArea,
    pub focus: ComposeField,
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
            ComposeField::Subject => ComposeField::Body,
            ComposeField::Body => ComposeField::To,
        };
    }

    pub fn prev_field(&mut self) {
        self.focus = match self.focus {
            ComposeField::To => ComposeField::Body,
            ComposeField::Cc => ComposeField::To,
            ComposeField::Bcc => ComposeField::Cc,
            ComposeField::Subject => ComposeField::Bcc,
            ComposeField::Body => ComposeField::Subject,
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
}

pub struct App {
    pub account_email: String,
    pub list_state: ListState,
    pub envelopes: Vec<Envelope>,
    pub selected: usize,
    pub body: BodyState,
    pub compose: Option<ComposeState>,
    pub status_message: Option<String>,
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
            should_quit: false,
        }
    }

    pub fn select_next(&mut self) {
        if self.envelopes.is_empty() {
            return;
        }
        self.selected = (self.selected + 1).min(self.envelopes.len() - 1);
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn selected_uid(&self) -> Option<u32> {
        self.envelopes.get(self.selected).map(|e| e.uid)
    }

    pub fn selected_envelope(&self) -> Option<&Envelope> {
        self.envelopes.get(self.selected)
    }
}
