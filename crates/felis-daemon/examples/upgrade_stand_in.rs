//! A successor for the Windows upgrade tests: it answers the version
//! probe the way `felis-daemon` does, then fails the hand-over the way
//! `FELIS_STAND_IN` names, so the tests can watch the predecessor roll
//! back. `exit` leaves before `ready`, `garbage` answers with a frame the
//! predecessor never asked for, and `silent` never answers.

#![allow(clippy::expect_used)]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("upgrade-probe") {
        std::process::exit(0);
    }
    #[cfg(windows)]
    stand_in(&args);
}

#[cfg(windows)]
fn stand_in(args: &[String]) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let channel = args
        .iter()
        .skip_while(|arg| *arg != "--resume")
        .nth(1)
        .expect("a --resume channel");
    let mode = std::env::var("FELIS_STAND_IN").unwrap_or_default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let (mut pipe, _server) =
            felis_transport::handover::connect_channel(channel).expect("connect the channel");
        let mut header = [0u8; 9];
        pipe.read_exact(&mut header).await.expect("the dump header");
        let mut len = [0u8; 8];
        len.copy_from_slice(&header[1..]);
        let mut dump = vec![0u8; usize::try_from(u64::from_le_bytes(len)).expect("a length")];
        pipe.read_exact(&mut dump).await.expect("the dump");
        match mode.as_str() {
            "exit" => std::process::exit(3),
            "garbage" => {
                pipe.write_all(&[0x7f, 0, 0, 0, 0, 0, 0, 0, 0])
                    .await
                    .expect("write");
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            }
            _ => tokio::time::sleep(std::time::Duration::from_secs(600)).await,
        }
    });
}
