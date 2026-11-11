mod capture;
mod portal;

use std::time::Duration;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Ask the portal for screen access (async: goes through D-Bus).
    let stream = portal::open().await?;
    println!(
        "Nœud PipeWire : {}, taille annoncée : {:?}",
        stream.node_id, stream.size
    );

    // 2. The PipeWire loop is blocking: run it on a dedicated thread
    //    so it doesn't block tokio's async runtime.
    let options = capture::Options {
        out_dir: "frames".into(),
        frame_count: 5,
        interval: Duration::from_millis(500),
    };
    tokio::task::spawn_blocking(move || capture::run(stream.node_id, stream.fd, options))
        .await??;

    println!("Terminé.");
    Ok(())
}
