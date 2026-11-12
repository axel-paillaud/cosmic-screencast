//! Only one recording at a time: running the command again stops it.
//!
//! This lets a single keyboard shortcut start and stop a recording (set in
//! COSMIC Settings > Keyboard > Custom shortcuts, like Print runs
//! `cosmic-screenshot`).
//!
//! It goes through D-Bus, the desktop's message bus. The first instance takes
//! a well-known name on the session bus and serves a `Stop` method. A second
//! instance finds the name already taken, calls `Stop`, and exits.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Notify;
use zbus::fdo::{RequestNameFlags, RequestNameReply};

/// Our name on the session bus (D-Bus names can't contain "-").
const BUS_NAME: &str = "com.github.cosmic_screencast";
const OBJECT_PATH: &str = "/com/github/cosmic_screencast";
const INTERFACE: &str = "com.github.cosmic_screencast.Control";

/// What this process should do.
pub enum Role {
    /// We are the only instance: go ahead and record.
    Recorder(Instance),
    /// Another instance was running: we asked it to stop.
    StopSent,
}

/// Keeps our bus name while alive, and tells us when a stop is requested.
pub struct Instance {
    /// Closing the connection releases the name.
    _connection: zbus::Connection,
    stop: Arc<Notify>,
}

impl Instance {
    /// Completes when another instance asked us to stop. A request made
    /// before this is called is not lost: `Notify` remembers one.
    pub async fn stop_requested(&self) {
        self.stop.notified().await;
    }
}

/// The object other instances call. `#[zbus::interface]` generates the
/// D-Bus plumbing: each method becomes callable on the bus.
struct Control {
    stop: Arc<Notify>,
}

#[zbus::interface(name = "com.github.cosmic_screencast.Control")]
impl Control {
    /// Stops the recording in progress (or cancels the selection).
    fn stop(&self) {
        self.stop.notify_one();
    }
}

/// Becomes the recorder, or stops the one already running.
pub async fn claim() -> Result<Role> {
    let connection = zbus::Connection::session()
        .await
        .context("cannot connect to the D-Bus session bus")?;

    let stop = Arc::new(Notify::new());
    connection
        .object_server()
        .at(OBJECT_PATH, Control { stop: stop.clone() })
        .await?;

    // DO_NOT_QUEUE alone: fail right away if taken. (zbus's default flags
    // would replace the running instance instead.)
    let reply = connection
        .request_name_with_flags(BUS_NAME, RequestNameFlags::DO_NOT_QUEUE)
        .await;
    match reply {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
            Ok(Role::Recorder(Instance {
                _connection: connection,
                stop,
            }))
        }
        // zbus reports "taken" as an error; `Exists` / `InQueue` are just in case.
        Err(zbus::Error::NameTaken) | Ok(RequestNameReply::Exists | RequestNameReply::InQueue) => {
            connection
                .call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), "Stop", &())
                .await
                .context("cannot stop the running instance")?;
            Ok(Role::StopSent)
        }
        Err(e) => Err(e).context("cannot register on the D-Bus session bus"),
    }
}
