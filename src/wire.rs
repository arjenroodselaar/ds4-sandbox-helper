//! Framing for both directions of the sandbox pipe:
//!
//! ```text
//! <decimal byte count>\n<exactly that many bytes of JSON>
//! ```
//!
//! The length is counted rather than delimited, so a payload may contain any byte
//! a JSON string can carry, newlines included.  The price is that a lost or extra
//! byte is unrecoverable: there is no marker to resynchronise on, and guessing
//! where the next frame starts would mean inventing tool results.  That is why
//! most failures here are fatal for the session and only the size ceilings are
//! not: a frame that is merely too big can still be counted out of the stream.

use std::fmt;
use std::io;

use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};

/// Longest accepted header, newline included.  A longer run of digits is not a
/// frame; it is a program printing something other than frames.
pub const MAX_HEADER_BYTES: usize = 32;
/// Largest request the agent will send, and largest response we will send.
/// Oversized answers are dropped rather than truncating a session.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// One frame's worth of bytes, or the fact that a frame was too large and was
/// counted out of the stream instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Payload(Vec<u8>),
    Skipped { declared: usize },
}

/// Why the stream can no longer be read as frames.  Every variant ends the
/// session: the peer is not speaking the protocol, or is gone.
#[derive(Debug)]
pub enum WireError {
    /// End of stream where a header had already begun: a truncated frame.
    EofMidHeader,
    /// End of stream where payload bytes were still owed.
    EofMidPayload {
        declared: usize,
        got: usize,
    },
    /// The bytes before the newline are not a positive decimal count.
    NotAByteCount {
        bytes: Vec<u8>,
    },
    /// A digit run longer than [`MAX_HEADER_BYTES`].
    HeaderTooLong {
        limit: usize,
    },
    Io(io::Error),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::EofMidHeader => write!(f, "stream ended in the middle of a frame header"),
            WireError::EofMidPayload { declared, got } => {
                write!(f, "stream ended after {got} of {declared} payload bytes")
            }
            WireError::NotAByteCount { bytes } => write!(
                f,
                "frame header is not a byte count: {:?}",
                String::from_utf8_lossy(bytes)
            ),
            WireError::HeaderTooLong { limit } => {
                write!(f, "frame header is over {limit} bytes")
            }
            WireError::Io(err) => write!(f, "sandbox stream io error: {err}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<io::Error> for WireError {
    fn from(err: io::Error) -> Self {
        WireError::Io(err)
    }
}

/// Reads the next frame, or `None` when the peer closed the stream cleanly at a
/// frame boundary.
///
/// A payload larger than `max_payload` is read and discarded and reported as
/// [`Frame::Skipped`], which keeps the stream aligned: the only thing wrong with
/// that frame was its size, and the request that provoked it can still be answered
/// with an error.
pub async fn read_frame<R>(reader: &mut R, max_payload: usize) -> Result<Option<Frame>, WireError>
where
    R: AsyncRead + AsyncBufRead + Unpin,
{
    let mut header = Vec::new();
    let read = reader.read_until(b'\n', &mut header).await?;
    if read == 0 {
        // Nothing at all: the agent closed stdin, which is how a session ends.
        return Ok(None);
    }
    if !header.ends_with(b"\n") {
        return Err(WireError::EofMidHeader);
    }
    header.pop();
    if header.len() > MAX_HEADER_BYTES {
        return Err(WireError::HeaderTooLong {
            limit: MAX_HEADER_BYTES,
        });
    }
    let declared = parse_header(&header).ok_or(WireError::NotAByteCount { bytes: header })?;

    if declared > max_payload {
        let mut sink = tokio::io::empty();
        tokio::io::copy(&mut reader.take(declared as u64), &mut sink).await?;
        return Ok(Some(Frame::Skipped { declared }));
    }

    let mut payload = vec![0u8; declared];
    let mut got = 0;
    while got < declared {
        let n = reader.read(&mut payload[got..]).await?;
        if n == 0 {
            return Err(WireError::EofMidPayload { declared, got });
        }
        got += n;
    }
    Ok(Some(Frame::Payload(payload)))
}

/// The digits before the newline, or `None` when they are not a positive decimal
/// count.  Leading zeros are accepted; an empty or signed or non-digit header is
/// not a frame.  The value is `usize`-sized on purpose: a count too large for
/// `usize` cannot be a frame either.
fn parse_header(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() || bytes.len() > 20 {
        return None;
    }
    if !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    match text.parse::<usize>() {
        Ok(0) | Err(_) => None,
        Ok(count) => Some(count),
    }
}

/// Writes one frame.  The caller owns the ceilings: a payload over the response
/// limit is an error to report, not something to squeeze into the stream.
pub async fn write_frame<W>(writer: &mut W, payload: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let header = format!("{}\n", payload.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(payload).await?;
    // The agent waits on this pipe; a buffered frame it cannot see looks exactly
    // like a sandbox that stopped thinking.
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::BufReader;

    async fn read(bytes: &[u8], max: usize) -> Result<Option<Frame>, WireError> {
        let mut reader = BufReader::new(Cursor::new(bytes.to_vec()));
        read_frame(&mut reader, max).await
    }

    #[test]
    fn header_parsing() {
        assert_eq!(parse_header(b"1"), Some(1));
        assert_eq!(parse_header(b"00042"), Some(42));
        assert_eq!(parse_header(b""), None, "an empty header is not a count");
        assert_eq!(parse_header(b"0"), None, "a frame always has a payload");
        assert_eq!(parse_header(b"-1"), None);
        assert_eq!(parse_header(b"12a"), None);
        assert_eq!(parse_header(b" 12"), None);
        assert_eq!(parse_header(&[b'9'; 21]), None, "more than 20 digits");
    }

    #[tokio::test]
    async fn round_trip() {
        let payload = br#"{"id":1,"ok":true,"result":"hi"}"#;
        let mut out: Vec<u8> = Vec::new();
        write_frame(&mut out, payload).await.unwrap();
        let header = format!("{}\n", payload.len());
        assert!(
            out.starts_with(header.as_bytes()),
            "header {:?}",
            String::from_utf8_lossy(&out)
        );
        let frame = read(&out, MAX_RESPONSE_BYTES).await.unwrap().unwrap();
        assert_eq!(frame, Frame::Payload(payload.to_vec()));
    }

    #[tokio::test]
    async fn clean_eof_at_a_frame_boundary() {
        assert!(read(b"", 100).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn truncated_header_is_fatal() {
        assert!(matches!(
            read(b"12", 100).await,
            Err(WireError::EofMidHeader)
        ));
    }

    #[tokio::test]
    async fn garbage_header_is_fatal() {
        let err = read(b"hello sandbox\n", 100).await.unwrap_err();
        assert!(err.to_string().contains("not a byte count"), "{err}");
        let err = read(b"\n", 100).await.unwrap_err();
        assert!(err.to_string().contains("not a byte count"), "{err}");
    }

    #[tokio::test]
    async fn short_payload_is_fatal() {
        let err = read(b"10\nabc", 100).await.unwrap_err();
        assert!(
            matches!(
                err,
                WireError::EofMidPayload {
                    declared: 10,
                    got: 3
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn oversized_frame_is_counted_out_and_the_stream_stays_aligned() {
        let body = vec![b'x'; 5_000];
        let mut bytes = format!("{}\n", body.len()).into_bytes();
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(b"2\n{}");

        let mut reader = BufReader::new(Cursor::new(bytes));
        let first = read_frame(&mut reader, 128).await.unwrap().unwrap();
        assert_eq!(first, Frame::Skipped { declared: 5_000 });
        let second = read_frame(&mut reader, 128).await.unwrap().unwrap();
        assert_eq!(second, Frame::Payload(b"{}".to_vec()));
    }

    #[tokio::test]
    async fn header_over_the_limit_is_fatal() {
        let bytes = [b'9'; 40];
        let mut with_newline = bytes.to_vec();
        with_newline.push(b'\n');
        assert!(matches!(
            read(&with_newline, MAX_RESPONSE_BYTES).await,
            Err(WireError::HeaderTooLong { .. })
        ));
    }

    #[tokio::test]
    async fn payload_is_byte_exact_with_multibyte_text() {
        let body = "é中😀".as_bytes().to_vec();
        let mut bytes = format!("{}\n", body.len()).into_bytes();
        bytes.extend_from_slice(&body);
        let frame = read(&bytes, MAX_RESPONSE_BYTES).await.unwrap().unwrap();
        assert_eq!(frame, Frame::Payload(body));
    }
}
