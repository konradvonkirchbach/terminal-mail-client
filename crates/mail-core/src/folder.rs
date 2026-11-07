//! Mailboxes ("folders") beyond INBOX: what the server lists, what each one
//! is for, and how its name reads to a human.

/// What a folder is for. Taken from the server's SPECIAL-USE attributes
/// (RFC 6154) where it sends them, guessed from common names otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FolderRole {
    Inbox,
    Drafts,
    Sent,
    Archive,
    Junk,
    Trash,
    /// A virtual "every message" view (Gmail's All Mail).
    All,
    /// A virtual "every flagged message" view (Gmail's Starred).
    Flagged,
}

impl FolderRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Drafts => "drafts",
            Self::Sent => "sent",
            Self::Archive => "archive",
            Self::Junk => "junk",
            Self::Trash => "trash",
            Self::All => "all",
            Self::Flagged => "flagged",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "inbox" => Self::Inbox,
            "drafts" => Self::Drafts,
            "sent" => Self::Sent,
            "archive" => Self::Archive,
            "junk" => Self::Junk,
            "trash" => Self::Trash,
            "all" => Self::All,
            "flagged" => Self::Flagged,
            _ => return None,
        })
    }

    /// Where folders with this role sort, before everything else by name:
    /// the inbox first, then the folders people go to most.
    fn rank(role: Option<Self>) -> u8 {
        match role {
            Some(Self::Inbox) => 0,
            Some(Self::Drafts) => 1,
            Some(Self::Sent) => 2,
            Some(Self::Archive) => 3,
            Some(Self::Flagged) => 4,
            Some(Self::All) => 5,
            Some(Self::Junk) => 6,
            Some(Self::Trash) => 7,
            None => 8,
        }
    }
}

/// A folder as cached locally.
#[derive(Debug, Clone, PartialEq)]
pub struct Folder {
    pub id: i64,
    pub account_id: i64,
    /// The server's name for it, exactly as IMAP commands need it
    /// (non-ASCII is in modified UTF-7 — see `display_name` for reading).
    pub name: String,
    /// The hierarchy separator, e.g. `/` or `.`; `None` for a flat
    /// namespace.
    pub delimiter: Option<String>,
    pub role: Option<FolderRole>,
    /// `false` for a `\Noselect` container that only holds other folders.
    pub selectable: bool,
    /// Unread messages on the server, as of the last folder refresh or
    /// sync of this folder. `None` until one has run.
    pub unseen: Option<u32>,
    pub total: Option<u32>,
}

impl Folder {
    /// The last component of the decoded name — what a folder list shows,
    /// indented by `depth`. INBOX reads as "Inbox".
    pub fn display_name(&self) -> String {
        if self.role == Some(FolderRole::Inbox) || self.name.eq_ignore_ascii_case("INBOX") {
            return "Inbox".to_string();
        }
        let decoded = decode_modified_utf7(&self.name);
        match self.delimiter.as_deref().filter(|d| !d.is_empty()) {
            Some(delim) => decoded.rsplit(delim).next().unwrap_or(&decoded).to_string(),
            None => decoded,
        }
    }

    /// How deeply nested the folder is (0 for a top-level one).
    pub fn depth(&self) -> usize {
        match self.delimiter.as_deref().filter(|d| !d.is_empty()) {
            Some(delim) => self.name.matches(delim).count(),
            None => 0,
        }
    }

    /// How far to indent the folder in a list sorted by `sort_key`. Folders
    /// with a role sort by role, away from their parent, so they aren't
    /// indented at all; the rest are by their depth — not counting an
    /// `INBOX` prefix, which on some servers (Dovecot's `INBOX.Sent`)
    /// every folder carries.
    pub fn indent(&self) -> usize {
        if self.role.is_some() {
            return 0;
        }
        let depth = self.depth();
        let under_inbox = self.delimiter.as_deref().is_some_and(|d| {
            !d.is_empty()
                && self.name.len() > 5 + d.len()
                && self.name[..5].eq_ignore_ascii_case("INBOX")
                && self.name[5..].starts_with(d)
        });
        if under_inbox {
            depth - 1
        } else {
            depth
        }
    }

    /// Sort key for a folder list: by role (`FolderRole::rank`), then by
    /// name, so that children follow their parent.
    pub fn sort_key(&self) -> (u8, String) {
        (FolderRole::rank(self.role), self.name.to_lowercase())
    }
}

/// One entry of a `LIST` response, before it's cached.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RemoteFolder {
    pub name: String,
    pub delimiter: Option<String>,
    pub role: Option<FolderRole>,
    pub selectable: bool,
}

/// Fills in roles the server didn't mark: INBOX by name, and for any role
/// no folder carries yet, the first folder whose name is a common one for
/// it — older servers (and some providers' localized folders) don't send
/// SPECIAL-USE attributes at all.
pub(crate) fn assign_roles(folders: &mut [RemoteFolder]) {
    for f in folders.iter_mut() {
        if f.name.eq_ignore_ascii_case("INBOX") {
            f.role = Some(FolderRole::Inbox);
        }
    }
    let guesses: [(FolderRole, &[&str]); 5] = [
        (
            FolderRole::Sent,
            &[
                "sent",
                "sent mail",
                "sent items",
                "sent messages",
                "gesendet",
                "gesendete objekte",
                "gesendete elemente",
            ],
        ),
        (
            FolderRole::Trash,
            &[
                "trash",
                "deleted items",
                "deleted messages",
                "bin",
                "papierkorb",
                "gelöschte objekte",
                "gelöschte elemente",
            ],
        ),
        (FolderRole::Drafts, &["drafts", "draft", "entwürfe"]),
        (FolderRole::Junk, &["junk", "spam", "junk e-mail", "junk-e-mail"]),
        (FolderRole::Archive, &["archive", "archiv"]),
    ];
    for (role, names) in guesses {
        if folders.iter().any(|f| f.role == Some(role)) {
            continue;
        }
        let found = names.iter().find_map(|wanted| {
            folders
                .iter()
                .position(|f| f.selectable && f.role.is_none() && leaf_name(f).to_lowercase() == *wanted)
        });
        if let Some(i) = found {
            folders[i].role = Some(role);
        }
    }
}

fn leaf_name(f: &RemoteFolder) -> String {
    let decoded = decode_modified_utf7(&f.name);
    match f.delimiter.as_deref().filter(|d| !d.is_empty()) {
        Some(delim) => decoded.rsplit(delim).next().unwrap_or(&decoded).to_string(),
        None => decoded,
    }
}

/// Decodes an IMAP mailbox name from modified UTF-7 (RFC 3501 §5.1.3):
/// `&...-` wraps base64 (with `,` for `/`) of UTF-16BE, and `&-` is a
/// literal `&`. A malformed sequence is left as it was rather than
/// dropped, so the name stays recognizable.
pub fn decode_modified_utf7(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('-') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let encoded = &after[..end];
        if encoded.is_empty() {
            out.push('&');
        } else {
            match decode_utf16_base64(encoded) {
                Some(text) => out.push_str(&text),
                None => out.push_str(&rest[start..start + end + 2]),
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn decode_utf16_base64(encoded: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(encoded.len() * 3 / 4);
    let (mut bits, mut nbits) = (0u32, 0u32);
    for c in encoded.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b',' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            bytes.push((bits >> nbits) as u8);
            bits &= (1 << nbits) - 1;
        }
    }
    // Whole UTF-16 code units only, with no stray bits left over.
    if nbits >= 6 || bits != 0 || bytes.is_empty() || bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(name: &str, role: Option<FolderRole>) -> RemoteFolder {
        RemoteFolder {
            name: name.to_string(),
            delimiter: Some("/".to_string()),
            role,
            selectable: true,
        }
    }

    fn folder(name: &str, delimiter: Option<&str>) -> Folder {
        Folder {
            id: 1,
            account_id: 1,
            name: name.to_string(),
            delimiter: delimiter.map(str::to_string),
            role: None,
            selectable: true,
            unseen: None,
            total: None,
        }
    }

    #[test]
    fn decodes_modified_utf7_folder_names() {
        assert_eq!(decode_modified_utf7("Entw&APw-rfe"), "Entwürfe");
        assert_eq!(decode_modified_utf7("Gel&APY-schte Elemente"), "Gelöschte Elemente");
        assert_eq!(decode_modified_utf7("Tom &- Jerry"), "Tom & Jerry");
        assert_eq!(decode_modified_utf7("plain"), "plain");
        assert_eq!(decode_modified_utf7("&ZeVnLIqe-"), "日本語");
    }

    #[test]
    fn leaves_malformed_utf7_readable() {
        assert_eq!(decode_modified_utf7("broken&AP"), "broken&AP");
        assert_eq!(decode_modified_utf7("odd&A-x"), "odd&A-x");
    }

    #[test]
    fn display_name_is_the_decoded_last_component() {
        assert_eq!(folder("[Gmail]/Sent Mail", Some("/")).display_name(), "Sent Mail");
        assert_eq!(folder("INBOX.Entw&APw-rfe", Some(".")).display_name(), "Entwürfe");
        assert_eq!(folder("INBOX", Some("/")).display_name(), "Inbox");
        assert_eq!(folder("Flat", None).display_name(), "Flat");
        assert_eq!(folder("a/b/c", Some("/")).depth(), 2);
        assert_eq!(folder("a/b/c", None).depth(), 0);
    }

    #[test]
    fn only_folders_without_a_role_are_indented_by_depth() {
        let mut sent = folder("[Gmail]/Sent Mail", Some("/"));
        sent.role = Some(FolderRole::Sent);
        assert_eq!(sent.indent(), 0, "sorted by role, away from its parent");
        assert_eq!(folder("[Gmail]/Important", Some("/")).indent(), 1);
        assert_eq!(folder("INBOX.Projects", Some(".")).indent(), 0);
        assert_eq!(folder("INBOX.Projects.2024", Some(".")).indent(), 1);
        assert_eq!(folder("Inboxes/Old", Some("/")).indent(), 1);
    }

    #[test]
    fn server_roles_win_and_names_fill_the_gaps() {
        let mut folders = vec![
            remote("INBOX", None),
            remote("Sent", None),
            remote("Sent Items", Some(FolderRole::Sent)),
            remote("Papierkorb", None),
            remote("Entw&APw-rfe", None),
        ];
        assign_roles(&mut folders);
        assert_eq!(folders[0].role, Some(FolderRole::Inbox));
        assert_eq!(folders[1].role, None, "the server already named a Sent folder");
        assert_eq!(folders[2].role, Some(FolderRole::Sent));
        assert_eq!(folders[3].role, Some(FolderRole::Trash));
        assert_eq!(folders[4].role, Some(FolderRole::Drafts));
    }

    #[test]
    fn role_names_survive_a_round_trip() {
        for role in [
            FolderRole::Inbox,
            FolderRole::Drafts,
            FolderRole::Sent,
            FolderRole::Archive,
            FolderRole::Junk,
            FolderRole::Trash,
            FolderRole::All,
            FolderRole::Flagged,
        ] {
            assert_eq!(FolderRole::parse(role.as_str()), Some(role));
        }
    }
}
