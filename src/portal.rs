//! Talks to the XDG ScreenCast portal (implemented by xdg-desktop-portal-cosmic).
//!
//! The portal gives us two things:
//! - a file descriptor (fd) to connect to PipeWire,
//! - the id of the PipeWire "node" streaming the selected screen.

use std::os::fd::OwnedFd;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ashpd::desktop::{PersistMode, Session};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};

/// Everything the capture needs to start.
pub struct PortalStream {
    pub node_id: u32,
    pub fd: OwnedFd,
    /// Size of the captured screen, in logical pixels (if the portal provides it).
    pub size: Option<(i32, i32)>,
    /// The portal session must stay alive while recording: if it's dropped,
    /// the D-Bus connection may close and the portal stops the stream.
    session: Session<Screencast>,
    _proxy: Screencast,
}

impl PortalStream {
    /// Tells the portal we're done (removes the "screen is shared" indicator).
    pub async fn close(self) -> Result<()> {
        self.session.close().await?;
        Ok(())
    }
}

/// File where the "restore token" is stored: it lets the portal remember
/// the selected screen and skip the selection dialog next time.
fn token_path() -> Option<PathBuf> {
    dirs::state_dir().map(|d| d.join("cosmic-screencast").join("restore_token"))
}

fn load_token() -> Option<String> {
    let path = token_path()?;
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

fn save_token(token: &str) -> Result<()> {
    let path = token_path().context("cannot find the ~/.local/state directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, token)?;
    Ok(())
}

/// Opens a screen capture session through the portal.
pub async fn open() -> Result<PortalStream> {
    let proxy = Screencast::new().await?;
    let session = proxy.create_session(Default::default()).await?;

    let token = load_token();
    println!(
        "Previous restore token: {}",
        if token.is_some() { "found" } else { "none" }
    );

    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                // The cursor is drawn into the image: handy for "click here" demos.
                .set_cursor_mode(CursorMode::EMBEDDED)
                .set_sources(SourceType::MONITOR)
                .set_multiple(false)
                .set_restore_token(token.as_deref())
                // Ask the portal to remember the choice until explicitly revoked.
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        )
        .await?;

    let response = proxy
        .start(&session, None, Default::default())
        .await?
        .response()
        .context("screen capture was denied or cancelled")?;

    // The portal returns a new token every time (the previous one becomes invalid).
    match response.restore_token() {
        Some(new_token) => {
            save_token(new_token)?;
            println!("New restore token saved.");
        }
        None => println!("The portal did not provide a restore token."),
    }

    let stream = response
        .streams()
        .first()
        .context("no screen selected")?
        .to_owned();

    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;

    Ok(PortalStream {
        node_id: stream.pipe_wire_node_id(),
        fd,
        size: stream.size(),
        session,
        _proxy: proxy,
    })
}
