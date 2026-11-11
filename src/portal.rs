//! Talks to the XDG ScreenCast portal (implemented by xdg-desktop-portal-cosmic).
//!
//! The portal gives us two things:
//! - a file descriptor (fd) to connect to PipeWire,
//! - the id of the PipeWire "node" streaming the selected screen.

use std::os::fd::OwnedFd;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};

/// Everything the capture needs to start.
pub struct PortalStream {
    pub node_id: u32,
    pub fd: OwnedFd,
    /// Size of the captured screen, in logical pixels (if the portal provides it).
    pub size: Option<(i32, i32)>,
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
    let path = token_path().context("impossible de trouver le dossier ~/.local/state")?;
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
        "Restore token précédent : {}",
        if token.is_some() { "oui" } else { "aucun" }
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
        .context("la capture a été refusée ou annulée")?;

    // The portal returns a new token every time (the previous one becomes invalid).
    match response.restore_token() {
        Some(new_token) => {
            save_token(new_token)?;
            println!("Nouveau restore token enregistré.");
        }
        None => println!("Le portail n'a pas fourni de restore token."),
    }

    let stream = response
        .streams()
        .first()
        .context("aucun écran sélectionné")?
        .to_owned();

    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;

    Ok(PortalStream {
        node_id: stream.pipe_wire_node_id(),
        fd,
        size: stream.size(),
    })
}
