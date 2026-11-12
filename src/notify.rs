//! Desktop notifications, the same way cosmic-screenshot does it: a direct
//! D-Bus call to `org.freedesktop.Notifications.Notify`.

use std::collections::HashMap;
use std::path::Path;

use zbus::Variant;

/// Name shown in the notification.
const APP_NAME: &str = "COSMIC Screencast";
/// Themed icon name. cosmic-screenshot uses its app ID, because it installs
/// an icon under that name; we don't have one yet, so use a standard icon.
const ICON: &str = "media-record";
/// How long the notification stays, in milliseconds.
const TIMEOUT_MS: i32 = 5000;

/// Generates `NotificationsProxy`: a Rust object whose methods are D-Bus calls.
/// `assume_defaults`: the interface name, service and path are the standard
/// `org.freedesktop.Notifications` ones.
#[zbus::proxy(assume_defaults = true)]
trait Notifications {
    /// Call the org.freedesktop.Notifications.Notify D-Bus method
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, &Variant<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

/// "Screencast saved to: <path>".
pub async fn saved(path: &Path) {
    // Transient: the notification doesn't stay in the notification history,
    // like cosmic-screenshot's.
    send(
        "Screencast saved to:",
        &path.to_string_lossy(),
        HashMap::from([("transient", &Variant::Bool(true))]),
    )
    .await;
}

/// "Screencast failed" + the error. Not transient: it stays in the history,
/// so it can be read later.
pub async fn failed(error: &anyhow::Error) {
    send("Screencast failed", &format!("{error:#}"), HashMap::new()).await;
}

/// A notification is a bonus: if it can't be sent, just say so.
async fn send(summary: &str, body: &str, hints: HashMap<&str, &Variant<'_>>) {
    let result = async {
        let connection = zbus::Connection::session().await?;
        NotificationsProxy::new(&connection)
            .await?
            .notify(APP_NAME, 0, ICON, summary, body, &[], hints, TIMEOUT_MS)
            .await
    }
    .await;
    if let Err(e) = result {
        eprintln!("Cannot send the notification: {e}");
    }
}
