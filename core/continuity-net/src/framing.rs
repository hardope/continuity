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

/// A single JPEG-encoded screen frame is comfortably under this even at
/// high resolution/quality — generous headroom while still bounding how
/// much a connection can make the receiver allocate for one frame.
const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Raw length-prefixed bytes, no JSON envelope — used for the screen
/// stream (see `Message::ScreenStreamHandshake`) and, since v3, a file's
/// data connection (`Message::FileStreamHandshake`, via
/// `write_frame_unflushed`), after that one JSON handshake message switches
/// the connection over to this leaner framing.
/// Skipping JSON/base64 here isn't just an optimization: a continuous
/// 10-15fps stream pays that ~1.33x base64 cost (fine for an occasional
/// file chunk) on every single frame, which is exactly the kind of
/// overhead "low latency" can't afford. Deliberately the same u32-BE
/// length-prefix shape as `write_message`/`read_message` above, just
/// without the JSON payload in between, so both framings stay trivially
/// easy to reason about side by side on the wire.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, data: &[u8]) -> Result<(), FramingError> {
    write_frame_unflushed(writer, data).await?;
    writer.flush().await?;
    Ok(())
}

/// `write_frame` without the flush — for a bulk stream where nothing is
/// waiting on any one frame (a v3 file data connection, see
/// `Message::FileStreamHandshake`), so frames can follow each other as fast
/// as the socket takes them. The screen stream keeps flushing per frame,
/// since there a frame sitting in a buffer is a frame shown late. Whoever
/// writes the last frame must flush (or follow it with `write_message`,
/// which does).
pub async fn write_frame_unflushed<W: AsyncWrite + Unpin>(writer: &mut W, data: &[u8]) -> Result<(), FramingError> {
    let len = u32::try_from(data.len()).map_err(|_| FramingError::TooLarge(u32::MAX))?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(data).await?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, FramingError> {
    read_frame_up_to(reader, MAX_FRAME_BYTES).await
}

/// `read_frame` for a stream whose frames have a smaller known bound (a
/// file's data connection: 1 MiB) — a longer one is refused before
/// anything is allocated for it.
pub async fn read_frame_up_to<R: AsyncRead + Unpin>(reader: &mut R, max_bytes: u32) -> Result<Vec<u8>, FramingError> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(FramingError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > max_bytes.min(MAX_FRAME_BYTES) {
        return Err(FramingError::TooLarge(len));
    }
    let mut data = vec![0u8; len as usize];
    reader.read_exact(&mut data).await?;
    Ok(data)
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

    /// What makes it safe for a reader to *skip* a `FramingError::Json`
    /// rather than drop the connection (see `PROTOCOL_VERSION` in
    /// continuity-proto): the length prefix means the whole unparseable
    /// payload has already been consumed by the time parsing fails, so
    /// the very next read starts cleanly at the following message.
    #[tokio::test]
    async fn an_unknown_message_type_leaves_the_stream_in_sync() {
        let (mut a, mut b) = tokio::io::duplex(4096);

        let unknown = br#"{"type":"some_future_message","field":1}"#;
        a.write_all(&(unknown.len() as u32).to_be_bytes()).await.unwrap();
        a.write_all(unknown).await.unwrap();
        write_message(&mut a, &Message::Ping).await.unwrap();

        let err = read_message(&mut b).await.unwrap_err();
        assert!(matches!(err, FramingError::Json(_)), "expected a JSON error, got {err:?}");
        assert!(matches!(read_message(&mut b).await.unwrap(), Message::Ping));
    }

    /// The v3 file data connection's whole shape on the wire: a JSON
    /// handshake, unflushed raw frames, a zero-length end-of-data frame,
    /// then JSON again — each read back in order, nothing lost to the
    /// missing per-frame flushes once the last JSON message flushes.
    #[tokio::test]
    async fn unflushed_frames_between_json_messages_read_back_in_order() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let writer = tokio::spawn(async move {
            write_message(&mut a, &Message::FileStreamHandshake { transfer_id: "t".into(), offset: 0 }).await.unwrap();
            for i in 0..3u8 {
                write_frame_unflushed(&mut a, &vec![i; 100_000]).await.unwrap();
            }
            write_frame_unflushed(&mut a, &[]).await.unwrap();
            write_message(&mut a, &Message::FileComplete { transfer_id: "t".into(), content_hash: "h".into() }).await.unwrap();
        });

        assert!(matches!(read_message(&mut b).await.unwrap(), Message::FileStreamHandshake { .. }));
        for i in 0..3u8 {
            assert_eq!(read_frame(&mut b).await.unwrap(), vec![i; 100_000]);
        }
        assert!(read_frame(&mut b).await.unwrap().is_empty());
        assert!(matches!(read_message(&mut b).await.unwrap(), Message::FileComplete { .. }));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn frame_bytes_round_trip_over_an_in_memory_duplex_stream() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let sent = vec![0xFFu8, 0x00, 0xAB, 0xCD, 0xEF];
        write_frame(&mut a, &sent).await.unwrap();
        let received = read_frame(&mut b).await.unwrap();
        assert_eq!(received, sent);
    }

    /// A frame over the caller's bound is refused from its length prefix
    /// alone, before the reader allocates or reads any of it.
    #[tokio::test]
    async fn a_frame_over_the_bound_is_refused_from_its_length() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        write_frame(&mut a, &[1u8; 10]).await.unwrap();
        assert_eq!(read_frame_up_to(&mut b, 10).await.unwrap(), vec![1u8; 10]);

        // Only the prefix is ever written: a reader that tried to read the
        // 11 bytes it announces would wait forever instead of failing.
        a.write_all(&11u32.to_be_bytes()).await.unwrap();
        assert!(matches!(read_frame_up_to(&mut b, 10).await.unwrap_err(), FramingError::TooLarge(11)));
    }
}
