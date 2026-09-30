//! Bounded clipboard-only frames. Never derive Debug for text-bearing messages.
use std::{io, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

pub(super) const MAX_TEXT: usize = 64 * 1024;
pub(super) const DEADLINE: Duration = Duration::from_secs(5);
const MAX_FRAME: usize = MAX_TEXT + 9;
pub(super) enum Message {
    State {
        send: bool,
        receive: bool,
        ready: bool,
    },
    Text {
        revision: u64,
        text: String,
    },
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid clipboard frame")
}
fn decode(bytes: &[u8]) -> io::Result<Message> {
    match bytes {
        [0, flags] if flags & !7 == 0 => Ok(Message::State {
            send: flags & 1 != 0,
            receive: flags & 2 != 0,
            ready: flags & 4 != 0,
        }),
        [1, rest @ ..] if rest.len() >= 8 && rest.len() <= MAX_TEXT + 8 => {
            let revision = u64::from_be_bytes(rest[..8].try_into().map_err(|_| invalid())?);
            if revision == 0 {
                return Err(invalid());
            }
            let text = std::str::from_utf8(&rest[8..])
                .map_err(|_| invalid())?
                .to_owned();
            Ok(Message::Text { revision, text })
        }
        _ => Err(invalid()),
    }
}

pub(super) async fn read(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Message> {
    // The complete frame, including its header, has a deadline. Heartbeats keep
    // idle sessions alive; a partial frame cannot hold resources indefinitely.
    timeout(DEADLINE, async {
        let len = reader.read_u32().await? as usize;
        if !(2..=MAX_FRAME).contains(&len) {
            return Err(invalid());
        }
        let mut bytes = vec![0; len];
        reader.read_exact(&mut bytes).await?;
        decode(&bytes)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "clipboard receive timeout"))?
}
pub(super) async fn write(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &Message,
) -> io::Result<()> {
    timeout(DEADLINE, async {
        match message {
            Message::State {
                send,
                receive,
                ready,
            } => {
                writer.write_u32(2).await?;
                writer
                    .write_all(&[
                        0,
                        u8::from(*send) | (u8::from(*receive) << 1) | (u8::from(*ready) << 2),
                    ])
                    .await?;
            }
            Message::Text { revision, text } => {
                if text.len() > MAX_TEXT || *revision == 0 {
                    return Err(invalid());
                }
                writer.write_u32((9 + text.len()) as u32).await?;
                writer.write_u8(1).await?;
                writer.write_u64(*revision).await?;
                writer.write_all(text.as_bytes()).await?;
            }
        }
        writer.flush().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "clipboard send timeout"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn unicode_and_maximum_roundtrip() {
        for text in [
            "Hello 🌍\nsecond line".into(),
            "x".repeat(MAX_TEXT),
            String::new(),
        ] {
            let mut buf = Vec::new();
            write(
                &mut buf,
                &Message::Text {
                    revision: 42,
                    text: text.clone(),
                },
            )
            .await
            .unwrap();
            let Message::Text {
                revision,
                text: actual,
            } = read(&mut buf.as_slice()).await.unwrap()
            else {
                panic!()
            };
            assert_eq!(revision, 42);
            assert_eq!(text, actual);
        }
    }
    #[tokio::test]
    async fn reject_unbounded_truncated_and_invalid_frames() {
        for bytes in [
            u32::MAX.to_be_bytes().to_vec(),
            vec![0, 0, 0, 2, 0, 8],
            vec![0, 0, 0, 9, 1, 0],
            vec![0, 0, 0, 10, 1, 0, 0, 0, 0, 0, 0, 0, 1, 255],
            vec![0, 0, 0, 9, 1, 0, 0, 0, 0, 0, 0, 0, 0],
        ] {
            assert!(read(&mut bytes.as_slice()).await.is_err());
        }
        assert!(
            write(
                &mut Vec::new(),
                &Message::Text {
                    revision: 1,
                    text: "x".repeat(MAX_TEXT + 1)
                }
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn incomplete_frames_expire() {
        let (mut sender, mut receiver) = tokio::io::duplex(64);
        sender.write_u32(9).await.unwrap();
        sender.write_u8(1).await.unwrap();
        let result = read(&mut receiver).await;
        assert!(matches!(result, Err(e) if e.kind() == io::ErrorKind::TimedOut));
    }
}
