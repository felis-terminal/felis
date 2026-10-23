//! Attach to a running session, print its screen as plain text, and detach:
//! `cargo run -p felis-client-core --example attach <session-id-prefix>`.
//! It dials the daemon `felis sessions list` would and never spawns one.
use std::error::Error;

use felis_client_core::local_socket::resolve_local_socket;
use felis_client_core::{AttachIntent, Offer, ShadowScreen, connect};
use felis_protocol::MessageKind;
use felis_protocol::messages::GridMsg;
use felis_transport::{Delivery, Incoming};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let prefix = std::env::args()
        .nth(1)
        .ok_or("usage: attach <session-id-prefix>")?;
    let socket = resolve_local_socket(None)?;
    // Not pull-paced: a paced client must request every frame, and this
    // one only wants the burst the attach replays.
    let mut conn = connect(socket.as_path(), Offer::window(false)).await?;
    let info = conn
        .attach_by_prefix(prefix, AttachIntent::Deliberate)
        .await?;
    let mut shadow = ShadowScreen::new(info.dims.rows, info.dims.cols);

    let mut burst_seen = false;
    while let Some(frame) = conn.next_frame().await? {
        // Not `codec::decode` on the raw body: the driver also refuses a
        // frame that is illegal for this side, phase or stream.
        let Incoming::Payload(payload) = conn.driver.classify(&frame)? else {
            continue;
        };
        if payload.kind != MessageKind::Grid {
            continue;
        }
        let Delivery::Deliver(delivered) = conn.driver.decode::<GridMsg>(&payload)? else {
            continue;
        };
        shadow.apply(&delivered.msg)?;
        burst_seen |= shadow.is_rehydrating();
        if burst_seen && !shadow.is_rehydrating() {
            break;
        }
    }

    let screen = shadow.screen();
    #[expect(clippy::print_stdout, reason = "the screen text is the output")]
    for row in 0..screen.rows() {
        let cells = screen.row_content(row).unwrap_or_default();
        println!(
            "{}",
            felis_grid::row_text_trim(cells, screen.cluster_table())
        );
    }
    Ok(())
}
