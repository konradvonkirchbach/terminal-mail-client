use mail_core::{Envelope, Message};

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

pub struct App {
    pub account_email: String,
    pub list_state: ListState,
    pub envelopes: Vec<Envelope>,
    pub selected: usize,
    pub body: BodyState,
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
}
