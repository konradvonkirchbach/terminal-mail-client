use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use tokio::sync::watch;

use super::{resolve, Theme};

/// Watches Omarchy's whole state directory (not just `colors.toml`
/// itself) and re-resolves the theme whenever anything in it changes.
/// Switching an Omarchy theme can repoint the `current` symlink rather
/// than editing `colors.toml` in place, which a watch on that one file's
/// path could miss when the underlying inode changes out from under it;
/// watching the tree and just re-reading on any event sidesteps that.
///
/// Runs its own OS thread for the life of the returned `watch::Receiver`
/// — dropping every clone of it stops the watcher.
pub fn spawn(mode: String) -> watch::Receiver<Theme> {
    let initial = resolve(&mode);
    let (tx, rx) = watch::channel(initial);

    let Some(state_dir) = directories::BaseDirs::new().and_then(|b| b.state_dir().map(|p| p.join("omarchy"))) else {
        return rx;
    };
    if !state_dir.exists() {
        // Not on Omarchy (or the theme system isn't set up) — the
        // bundled fallback palette from `resolve()` above is already in
        // the channel; nothing to watch.
        return rx;
    }

    std::thread::spawn(move || {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = event_tx.send(res);
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("theme watcher setup failed: {e}");
                return;
            }
        };
        if let Err(e) = watcher.watch(&state_dir, RecursiveMode::Recursive) {
            tracing::warn!("theme watcher failed to watch {state_dir:?}: {e}");
            return;
        }

        for res in event_rx.iter() {
            if res.is_err() {
                continue;
            }
            // A theme switch touches several files in quick succession
            // (and possibly repoints a symlink); give it a moment to
            // settle, then drain anything else that piled up meanwhile
            // so a whole switch coalesces into a single reload.
            std::thread::sleep(Duration::from_millis(150));
            while event_rx.try_recv().is_ok() {}

            if tx.send(resolve(&mode)).is_err() {
                return; // receiving end dropped — app is shutting down
            }
        }
    });

    rx
}
