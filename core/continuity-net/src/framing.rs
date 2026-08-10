use continuity_proto::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Generous enough for file chunks, small enough to bound how much an
/// unauthenticated-but-connected peer can make us allocate before pairing
/// completes.
const MAX_MESSAGE_BYTES: u32 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    #[error("connection closed")]
    Closed,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("message of {0} bytes exceeds the {MAX_MESSAGE_BYTES} byte limit")]
    TooLarge(u32),
    #[error("malformed message: {0}")]
    Json(#[from] serde_json::Error),
}

/// Writes one `Message` as a u32-BE length prefix followed by its JSON
/// encoding. Framing lives here (not in `continuity-proto`) so the schema
/// crate stays free of I/O and async-runtime dependencies.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Message,
) -> Result<(), FramingError> {
    let payload = serde_json::to_vec(msg)?;
    let len = u32::try_from(payload.len()).map_err(|_| FramingError::TooLarge(u32::MAX))?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Message, FramingError> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(FramingError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_MESSAGE_BYTES {
        return Err(FramingError::TooLarge(len));
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(serde_json::from_slice(&payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use continuity_proto::Message;

    #[tokio::test]
    async fn round_trips_over_an_in_memory_duplex_stream() {
        let (mut a, mut b) = tokio::io::duplex(4096);

        let sent = Message::Ping;
        write_message(&mut a, &sent).await.unwrap();

        let received = read_message(&mut b).await.unwrap();
        matches!(received, Message::Ping);
    }

    #[tokio::test]
    async fn closed_stream_yields_closed_error() {
        let (a, mut b) = tokio::io::duplex(4096);
        drop(a);
        let err = read_message(&mut b).await.unwrap_err();
        assert!(matches!(err, FramingError::Closed));
    }
}
