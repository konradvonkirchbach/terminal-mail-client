//! Desktop notifications for newly-arrived mail, via the freedesktop
//! notification D-Bus interface (whatever daemon the desktop runs — mako,
//! dunst, etc. all speak this). Only called for genuinely new mail found
//! by the IDLE loop, never for the initial backfill sync or a manual
//! refresh, so it doesn't spam the user's unread backlog on launch.

use mail_core::Envelope;

const MAX_PREVIEW: usize = 3;

pub fn notify_new_mail(account_email: &str, envelopes: &[Envelope]) {
    if envelopes.is_empty() {
        return;
    }

    let (summary, body) = if let [only] = envelopes {
        let from = sender_display(only);
        (
            format!("New mail — {account_email}"),
            format!("{from}\n{}", only.subject),
        )
    } else {
        let mut body = envelopes
            .iter()
            .take(MAX_PREVIEW)
            .map(|e| format!("{}: {}", sender_display(e), e.subject))
            .collect::<Vec<_>>()
            .join("\n");
        if envelopes.len() > MAX_PREVIEW {
            body.push_str(&format!("\n+{} more", envelopes.len() - MAX_PREVIEW));
        }
        (format!("{} new messages — {account_email}", envelopes.len()), body)
    };

    // notify-rust's `show()` is a blocking D-Bus round-trip; keep it off
    // the event loop.
    tokio::task::spawn_blocking(move || {
        let result = notify_rust::Notification::new()
            .appname("mailc")
            .summary(&summary)
            .body(&body)
            .show();
        if let Err(e) = result {
            tracing::warn!("desktop notification failed: {e}");
        }
    });
}

fn sender_display(e: &Envelope) -> String {
    e.from
        .first()
        .map(|a| a.name.clone().unwrap_or_else(|| a.email.clone()))
        .unwrap_or_else(|| "(unknown sender)".to_string())
}
