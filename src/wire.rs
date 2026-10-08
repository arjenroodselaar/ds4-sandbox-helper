// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Framing for both directions of the sandbox pipe:
//!
//! ```text
//! <decimal byte count>\n<exactly that many bytes of JSON>
//! ```
//!
//! A counted length lets a payload carry any byte a JSON string can.  The price is
//! that a lost or extra byte cannot be resynchronised on, so most failures here end
//! the session.  An oversized frame is the exception, since it can be counted out, and
//! only for as long as its bytes keep arriving.

use std::fmt;
use std::io;

use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

/// Longest header read before it is called too long, newline excluded.  The spec allows
/// 1-20 digits, which `parse_header` enforces, so this only bounds a digit run that
/// cannot be a count in the first place.
pub const MAX_HEADER_BYTES: usize = 32;
/// Largest request accepted, and largest response sent.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// One frame's bytes, or the fact that it was too large and was counted out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Payload(Vec<u8>),
    Skipped { declared: usize },
}

/// Why the stream can no longer be read as frames.  Every variant ends the session.
#[derive(Debug)]
pub enum WireError {
    /// End of stream where a header had already begun, which is a truncated frame.
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

/// Reads the next frame, or `None` when the peer closed the stream at a frame boundary.
///
/// A payload over `max_payload` is counted out and reported as [`Frame::Skipped`],
/// which keeps the stream aligned.
pub async fn read_frame<R>(reader: &mut R, max_payload: usize) -> Result<Option<Frame>, WireError>
where
    R: AsyncRead + AsyncBufRead + Unpin,
{
    // Read one byte past the longest header, so a run of digits that can never be a
    // count is stopped at the limit instead of buffered whole.  A `Take` never hands out
    // a byte beyond its limit, newline or no newline.
    let limit = MAX_HEADER_BYTES + 1;
    let mut header = Vec::with_capacity(limit);
    let read = (&mut *reader)
        .take(limit as u64)
        .read_until(b'\n', &mut header)
        .await?;
    if read == 0 {
        return Ok(None);
    }
    if !header.ends_with(b"\n") {
        // Past the limit there is no count the rest could turn into, so the run is too
        // long whether or not a newline follows it.  Short of the limit, the stream
        // stopped in the middle of a count.
        if read == limit {
            return Err(WireError::HeaderTooLong {
                limit: MAX_HEADER_BYTES,
            });
        }
        return Err(WireError::EofMidHeader);
    }
    header.pop();
    let declared = parse_header(&header).ok_or(WireError::NotAByteCount { bytes: header })?;

    if declared > max_payload {
        // Too large is not the same as unfinished.  A frame that stops early is the one
        // failure a counted length cannot recover from, so a skipped payload that runs
        // out is fatal like any other, rather than a clean end of session.
        let mut sink = tokio::io::empty();
        let got = tokio::io::copy(&mut reader.take(declared as u64), &mut sink).await?;
        if got != declared as u64 {
            return Err(WireError::EofMidPayload {
                declared,
                got: got as usize,
            });
        }
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

/// The digits before the newline.  Leading zeros are accepted.  An empty, signed or
/// non-digit header is not a count, nor is one too large for `usize`.
fn parse_header(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() || bytes.len() > 20 {
        return None;
    }
    let mut count = 0usize;
    for byte in bytes {
        // Anything below `0` underflows and anything above `9` is a digit past ten, so
        // this is the whole of the digit test without a pass of its own.
        let digit = byte.checked_sub(b'0')?;
        if digit > 9 {
            return None;
        }
        count = count.checked_mul(10)?.checked_add(usize::from(digit))?;
    }
    // A frame always has a payload, and a peer that wants to say nothing closes the
    // stream instead.
    if count == 0 {
        return None;
    }
    Some(count)
}

/// Writes one frame.  The caller owns the ceilings.
pub async fn write_frame<W>(writer: &mut W, payload: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let header = format!("{}\n", payload.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(payload).await?;
    // The agent waits on this pipe, so a buffered frame looks like a stuck sandbox.
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::pin::Pin;
    use std::task::Context;
    use std::task::Poll;
    use tokio::io::BufReader;
    use tokio::io::ReadBuf;

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
        // Twenty digits is inside the length a header may have and outside what a
        // `usize` holds, and adding them up has to notice which.
        assert_eq!(parse_header(b"99999999999999999999"), None);
        assert_eq!(parse_header(b"4000000000"), Some(4_000_000_000));
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

    /// The limit is inclusive, so a request that fills it exactly is a request and not
    /// an oversized one.
    #[tokio::test]
    async fn a_payload_of_exactly_the_limit_is_read_whole() {
        let body = vec![b'x'; MAX_REQUEST_BYTES];
        let mut bytes = format!("{}\n", body.len()).into_bytes();
        bytes.extend_from_slice(&body);
        let frame = read(&bytes, MAX_REQUEST_BYTES).await.unwrap().unwrap();
        assert_eq!(frame, Frame::Payload(body));
    }

    /// And one byte past it is counted out with the stream still aligned, because the
    /// size is the only thing the helper could not accept.
    #[tokio::test]
    async fn one_byte_past_the_limit_is_counted_out_and_the_stream_stays_aligned() {
        let body = vec![b'x'; MAX_REQUEST_BYTES + 1];
        let mut bytes = format!("{}\n", body.len()).into_bytes();
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(b"2\n{}");

        let mut reader = BufReader::new(Cursor::new(bytes));
        let first = read_frame(&mut reader, MAX_REQUEST_BYTES)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            first,
            Frame::Skipped {
                declared: MAX_REQUEST_BYTES + 1
            }
        );
        let second = read_frame(&mut reader, MAX_REQUEST_BYTES)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second, Frame::Payload(b"{}".to_vec()));
    }

    /// A skipped payload is still a payload.  Stopping in the middle of one is the one
    /// thing a counted length cannot be recovered from, whatever its size was meant to
    /// be, so it is not read as a clean end of session.
    #[tokio::test]
    async fn eof_in_the_middle_of_a_skipped_frame_is_fatal() {
        let err = read(b"5000\nshort", 128).await.unwrap_err();
        assert!(
            matches!(
                err,
                WireError::EofMidPayload {
                    declared: 5_000,
                    got: 5
                }
            ),
            "{err:?}"
        );
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

    /// Digits with no newline in them, which stops answering after `stop_after` bytes.
    /// A header read that follows the run until its newline would run into that stop and
    /// say so, so reaching `HeaderTooLong` proves the read stopped by itself.
    struct EndlessDigits {
        handed: u64,
        stop_after: u64,
    }

    impl AsyncRead for EndlessDigits {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let me = self.get_mut();
            if me.handed >= me.stop_after {
                let err = io::Error::other("asked for more header than the limit allows");
                return Poll::Ready(Err(err));
            }
            let space = buf.initialize_unfilled();
            let n = space.len().min((me.stop_after - me.handed) as usize);
            space[..n].fill(b'9');
            me.handed += n as u64;
            buf.advance(n);
            Poll::Ready(Ok(()))
        }
    }

    /// The peer does not have to send a newline to make the helper hold a megabyte of
    /// digits, and it must not be able to make it hold anything at all.
    #[tokio::test]
    async fn a_run_of_digits_is_stopped_at_the_limit() {
        let mut reader = BufReader::new(EndlessDigits {
            handed: 0,
            stop_after: MAX_HEADER_BYTES as u64 + 1,
        });
        let err = read_frame(&mut reader, MAX_RESPONSE_BYTES)
            .await
            .unwrap_err();
        assert!(
            matches!(err, WireError::HeaderTooLong { .. }),
            "read past the limit: {err}"
        );
    }

    /// Where a digit run stops being a count the helper can quote and becomes a header it
    /// will not read.  The spec's own count is 20 digits, and the wire allows 32 bytes.
    #[tokio::test]
    async fn a_long_digit_run_says_which_limit_it_crossed() {
        for digits in 21..=MAX_HEADER_BYTES {
            let mut bytes = vec![b'9'; digits];
            bytes.push(b'\n');
            let err = read(&bytes, MAX_RESPONSE_BYTES).await.unwrap_err();
            assert!(
                err.to_string().contains("not a byte count"),
                "{digits} digits: {err}"
            );
        }
        let mut bytes = vec![b'9'; MAX_HEADER_BYTES + 1];
        bytes.push(b'\n');
        assert!(matches!(
            read(&bytes, MAX_RESPONSE_BYTES).await,
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
