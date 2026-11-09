//! Framing: a big-endian `u32` length, then that many bytes of JSON.

use std::io;

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest frame either side accepts, 16 MiB.
pub const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// Write `msg` as one frame and flush. A message over `MAX_FRAME` is an `InvalidInput`
/// error and nothing is written.
pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut frame = vec![0; 4];
    serde_json::to_writer(&mut frame, msg).map_err(io::Error::other)?;
    let len = u32::try_from(frame.len() - 4)
        .ok()
        .filter(|len| *len <= MAX_FRAME)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "message of {} bytes is over the frame limit",
                    frame.len() - 4
                ),
            )
        })?;
    frame[..4].copy_from_slice(&len.to_be_bytes());
    w.write_all(&frame).await?;
    w.flush().await
}

/// Read one frame; `None` on a clean end of stream (before any byte of a frame). A
/// declared length over `MAX_FRAME`, or JSON that isn't a `T`, is an `InvalidData` error;
/// an over-long frame is refused before anything is allocated for it.
pub async fn read_frame<R, T>(r: &mut R) -> io::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut header = [0; 4];
    let mut filled = 0;
    while filled < header.len() {
        match r.read(&mut header[filled..]).await? {
            0 if filled == 0 => return Ok(None),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => filled += n,
        }
    }
    let len = u32::from_be_bytes(header);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes is over the limit"),
        ));
    }
    let mut body = vec![0; len as usize];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use tokio::io::duplex;

    use super::*;
    use crate::proto::{Event, Level, Notice, ServerMsg};

    fn notice() -> ServerMsg {
        ServerMsg::Event(Event::Notice(Notice {
            id: 7,
            level: Level::Error,
            text: "Soloist stopped".into(),
        }))
    }

    #[tokio::test]
    async fn frame_roundtrip() {
        let (mut a, mut b) = duplex(16);
        let msg = notice();
        let (written, read) = tokio::join!(
            async {
                write_frame(&mut a, &msg).await?;
                write_frame(&mut a, &msg).await
            },
            async {
                let first = read_frame::<_, ServerMsg>(&mut b).await?;
                let second = read_frame::<_, ServerMsg>(&mut b).await?;
                io::Result::Ok((first, second))
            }
        );
        written.unwrap();
        let (first, second) = read.unwrap();
        assert_eq!(first, Some(msg.clone()));
        assert_eq!(second, Some(msg));
    }

    #[tokio::test]
    async fn frame_is_length_then_json() {
        let mut out = Vec::new();
        write_frame(&mut out, &serde_json::json!({"a": 1}))
            .await
            .unwrap();
        assert_eq!(out, b"\0\0\0\x07{\"a\":1}");
    }

    #[tokio::test]
    async fn clean_eof_is_none() {
        let mut empty: &[u8] = &[];
        assert_eq!(read_frame::<_, ServerMsg>(&mut empty).await.unwrap(), None);
    }

    #[tokio::test]
    async fn eof_inside_a_frame_is_an_error() {
        let mut half_length: &[u8] = &[0, 0];
        let err = read_frame::<_, ServerMsg>(&mut half_length)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

        let mut half_body: &[u8] = b"\0\0\0\x07{\"a\"";
        let err = read_frame::<_, serde_json::Value>(&mut half_body)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn oversize_frame_rejected() {
        // Only the length arrives: refusing it must not read (or allocate) a body, which
        // would end in UnexpectedEof instead.
        let length = 0x0100_0001u32.to_be_bytes();
        let err = read_frame::<_, ServerMsg>(&mut &length[..])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn oversize_message_not_written() {
        let mut out = Vec::new();
        let big = "x".repeat(MAX_FRAME as usize);
        let err = write_frame(&mut out, &big).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn bad_json_is_invalid_data() {
        let mut frame: &[u8] = b"\0\0\0\x02{}";
        let err = read_frame::<_, ServerMsg>(&mut frame).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
