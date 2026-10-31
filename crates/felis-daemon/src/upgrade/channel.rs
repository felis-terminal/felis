//! The frames a Windows hand-over exchanges on its private pipe: a tag
//! and a length before each payload, so the dump never relies on the
//! pipe closing and the channel stays open for the commit.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Predecessor to successor: the dump, its handle values filled in.
pub(super) const DUMP: u8 = 1;
/// Successor to predecessor: everything is adopted and listening, and
/// nothing has started.
pub(super) const READY: u8 = 2;
/// Predecessor to successor: the predecessor is leaving and will not
/// resume.
pub(super) const COMMIT: u8 = 3;
/// Successor to predecessor, instead of `READY`: why it gave up.
pub(super) const FAILED: u8 = 4;

/// No dump comes near this; a length past it is not a frame this
/// channel sent.
const MAX_PAYLOAD: u64 = 1 << 34;

pub(super) async fn send(
    pipe: &mut (impl AsyncWrite + Unpin),
    tag: u8,
    payload: &[u8],
) -> io::Result<()> {
    let mut header = [0u8; 9];
    header[0] = tag;
    header[1..].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    pipe.write_all(&header).await?;
    pipe.write_all(payload).await?;
    pipe.flush().await
}

pub(super) async fn recv(pipe: &mut (impl AsyncRead + Unpin)) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 9];
    pipe.read_exact(&mut header).await?;
    let mut len = [0u8; 8];
    len.copy_from_slice(&header[1..]);
    let len = u64::from_le_bytes(len);
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a {len}-byte upgrade frame"),
        ));
    }
    let mut payload = vec![0u8; usize::try_from(len).map_err(io::Error::other)?];
    pipe.read_exact(&mut payload).await?;
    Ok((header[0], payload))
}

/// Receives one frame and requires its tag.
pub(super) async fn expect(pipe: &mut (impl AsyncRead + Unpin), tag: u8) -> io::Result<Vec<u8>> {
    let (got, payload) = recv(pipe).await?;
    if got != tag {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected upgrade frame {tag}, got {got}"),
        ));
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_arrive_whole_and_in_order() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            send(&mut a, DUMP, &[7u8; 1000]).await.unwrap();
            send(&mut a, COMMIT, &[]).await.unwrap();
        });
        assert_eq!(expect(&mut b, DUMP).await.unwrap(), vec![7u8; 1000]);
        assert_eq!(recv(&mut b).await.unwrap(), (COMMIT, Vec::new()));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn an_unexpected_tag_or_a_closed_channel_is_an_error() {
        let (mut a, mut b) = tokio::io::duplex(64);
        send(&mut a, FAILED, b"no").await.unwrap();
        drop(a);
        assert!(expect(&mut b, READY).await.is_err());
        assert!(recv(&mut b).await.is_err(), "the channel closed");
    }

    #[tokio::test]
    async fn a_header_announcing_more_than_the_payload_limit_is_invalid_data() {
        let mut header = vec![DUMP];
        header.extend_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        let err = recv(&mut header.as_slice()).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
