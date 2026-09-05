# terminal-mail-client

A fast, minimalist, keyboard-only email client for the terminal — built to
feel at home on [Omarchy](https://omarchy.org/) (Hyprland + Arch), but works
on any Linux desktop.

Reads its color palette live from Omarchy's active theme, updates instantly
if you switch themes while it's running, and integrates with the desktop as
a first-class app: a launcher entry, a global keybinding, and native
notifications for new mail.

## Features

- **IMAP/SMTP** — works with any standard provider (Gmail, Yahoo, Fastmail,
  self-hosted, ...) via password or app-password auth
- **Multi-account** — add as many as you like, switch with `Tab` or a number key
- **Instant + offline** — a local SQLite cache means launch is instant and
  reading works offline; syncs in the background
- **Push updates** — a persistent IMAP `IDLE` connection per account means
  new mail shows up on its own, no polling
- **Full compose flow** — reply, attachments (with a keyboard-driven
  directory browser, not blind path typing), a 20MB size cap
- **Attachment downloads** — same directory browser, in reverse
- **Search** — live filter the inbox by sender or subject
- **Omarchy-native theming** — reads the active theme's `colors.toml` and
  live-reloads if you switch themes; falls back to a bundled palette
  everywhere else
- **Desktop integration** — `.desktop` launcher entry, a Hyprland
  keybinding, and native notifications for new mail

## Not yet built

- OAuth2 (Gmail/Microsoft) — currently uses app-password auth instead, which
  works today but is a smaller lift than a full OAuth2 flow
- A GUI frontend — the core (`mail-core`) is deliberately UI-agnostic for this

## Installation

Requires a Rust toolchain ([rustup.rs](https://rustup.rs)).

```sh
git clone https://github.com/<you>/terminal-mail-client.git
cd terminal-mail-client
cargo install --path crates/mail-tui
```

This installs the `mailc` binary to `~/.cargo/bin` (make sure that's on your
`PATH`).

### Desktop integration (optional, Omarchy/Hyprland)

```sh
cp packaging/mailc.desktop ~/.local/share/applications/
```

Gives you a launcher entry that opens `mailc` in a floating terminal window
(via Omarchy's `omarchy-launch-or-focus-tui`) and focuses the existing window
instead of duplicating it if launched again.

For a global keybinding, add this to `~/.config/hypr/bindings.lua`:

```lua
o.bind("SUPER + M", "Mail", { tui = "mailc", focus = true })
```

(Pick a different key if `SUPER + M` is already bound to something on your
system — check with `omarchy menu keybindings --print`.)

## Getting started

First run walks you through adding an account:

```sh
mailc
```

You'll need your IMAP/SMTP host and port (e.g. `imap.gmail.com` / 993 and
`smtp.gmail.com` / 587 for Gmail) and a password. Most providers with 2FA
enabled require an **app password** rather than your normal one — Gmail,
Yahoo, and most others generate these under Account Security settings.

Add more accounts later:

```sh
mailc --add-account
```

Update a stored password (e.g. after rotating an app password):

```sh
mailc --set-password
```

## Keybindings

| Key | Action |
|---|---|
| `j` / `k`, `↓` / `↑`, `n` / `N` | Move selection |
| `Enter` | Read selected message |
| `/` | Search (live filter by sender/subject); `Enter` confirms, `Esc` clears |
| `c` | Compose a new message |
| `r` | Reply to selected message |
| `[` / `]` | Cycle selected attachment on an open message |
| `a` | Download selected attachment |
| `S` | Manual sync |
| `Tab` / `Shift+Tab` | Switch account (when more than one is configured) |
| `1`–`9` | Jump directly to account N |
| `q` / `Esc` | Quit |

**Compose:**

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Move between To/Cc/Bcc/Subject/Files/Body |
| `Ctrl+A` | Attach a file (opens a directory browser) |
| `Ctrl+S` | Send |
| `Backspace` (on Files field) | Remove selected attachment |
| `Esc` | Cancel |

**Directory browser** (attach a file / choose a save location):

| Key | Action |
|---|---|
| `j` / `k` | Move selection |
| `Enter` | Open directory / pick file, or confirm a save |
| `Tab` | Switch focus between the file list and the filename field (save mode) |
| `Esc` | Cancel |

## Architecture

Two crates:

- **`mail-core`** — IMAP/SMTP protocol handling, the local SQLite cache, sync
  engine, IDLE push loop, and sending. No UI dependencies at all — this is
  what a future GUI frontend would reuse instead of the TUI.
- **`mail-tui`** — the [ratatui](https://ratatui.rs)-based terminal
  interface: rendering, keybindings, Omarchy theme integration, desktop
  notifications.

## License

MIT — see [LICENSE](LICENSE).
