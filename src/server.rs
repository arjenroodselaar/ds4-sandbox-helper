//! The request loop: one frame in, one frame out, in order.
//!
//! The agent keeps exactly one request in flight, which is what lets this be a loop
//! instead of a multiplexer: there is never more than one answer outstanding, so
//! responses cannot arrive out of order and no request can starve another.  A tool
//! that takes four minutes (a `bash` command) is still a single in-flight request, and
//! the concurrency a long command needs lives inside the tool, not here.
//!
//! Every way this loop can go wrong ends the session rather than being worked around.
//! A frame we cannot read means the byte counts have stopped agreeing with the peer,
//! and a request with no id cannot be answered in a way the agent would accept; in
//! either case the agent is better off being told now than waiting for an answer that
//! would be a guess.

use std::io::ErrorKind;

use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::BufReader;

use crate::protocol;
use crate::protocol::notice;
use crate::protocol::response_error;
use crate::protocol::response_ok;
use crate::tools;
use crate::tools::Config;
use crate::tools::Session;
use crate::wire;
use crate::wire::Frame;
use crate::wire::MAX_REQUEST_BYTES;
use crate::wire::MAX_RESPONSE_BYTES;

/// Why the loop stopped.  The exit code and the stderr line both come from this.
pub enum Outcome {
    /// The agent closed the pipe.  Normal end of a run.
    Finished,
    /// The peer stopped making sense.  Worth a nonzero exit and a reason.
    Fault(String),
}

pub async fn serve<R, W>(reader: R, mut writer: W, config: Config) -> Outcome
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut session = Session::default();

    // Diagnostics travel as id-0 notices, which the agent mirrors into its sandbox
    // log next to the helper's stderr.  The alternative, printing to stdout, would be
    // a frame the agent cannot parse.
    let startup = notice(&format!(
        // The read size is named as a default because that is all it is: the agent
        // sends the real number with each request, and this only matters to a sender
        // that does not.
        "ds4-sandbox-helper {} ready: read_lines default {}, edit_upto={}",
        env!("CARGO_PKG_VERSION"),
        config.read_lines,
        config.edit_upto
    ));
    if write(&mut writer, &startup).await.is_err() {
        session.finish().await;
        return Outcome::Finished;
    }

    loop {
        let frame = match wire::read_frame(&mut reader, MAX_REQUEST_BYTES).await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                session.finish().await;
                return Outcome::Finished;
            }
            Err(err) => {
                session.finish().await;
                return Outcome::Fault(err.to_string());
            }
        };

        let payload = match frame {
            Frame::Payload(payload) => payload,
            Frame::Skipped { declared } => {
                // There is no id to answer to, so the most useful thing left is to say
                // what happened and stay byte-aligned for whatever comes next.
                let _ = write(
                    &mut writer,
                    &notice(&format!(
                        "request of {declared} bytes exceeds the {MAX_REQUEST_BYTES} byte limit and was dropped"
                    )),
                )
                .await;
                continue;
            }
        };

        let request = match protocol::parse_request(&payload) {
            Ok(request) => request,
            Err(err) => {
                session.finish().await;
                return Outcome::Fault(format!("{err}: {}", truncate_for_log(&payload)));
            }
        };

        let reply = match tools::run(&request, &mut session, &config).await {
            Ok(result) => response_ok(request.id, &result),
            Err(message) => response_error(request.id, &message),
        };
        // A tool that ignored its byte budget would produce a frame the agent refuses
        // outright.  Replacing it with a short error keeps the conversation going.
        let reply = if reply.len() > MAX_RESPONSE_BYTES {
            response_error(
                request.id,
                &format!(
                    "output was {} bytes, over the {} byte response limit",
                    reply.len(),
                    MAX_RESPONSE_BYTES
                ),
            )
        } else {
            reply
        };

        if let Err(err) = write(&mut writer, &reply).await {
            session.finish().await;
            // The agent exits as soon as the run ends, and a sandbox still writing
            // its last answer at that moment sees EPIPE.  That is a normal ending.
            return if err.kind() == ErrorKind::BrokenPipe {
                Outcome::Finished
            } else {
                Outcome::Fault(format!("writing a response: {err}"))
            };
        }
    }
}

async fn write<W>(writer: &mut W, payload: &[u8]) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    wire::write_frame(writer, payload).await
}

/// A request that could not be parsed is quoted in the exit message, shortened: the
/// point is to recognise the shape of what arrived, not to fill a terminal.
fn truncate_for_log(payload: &[u8]) -> String {
    let text = String::from_utf8_lossy(payload);
    let cut = text
        .char_indices()
        .nth(200)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    let mut quoted = text[..cut].replace('\n', " ");
    if cut < text.len() {
        quoted.push('…');
    }
    quoted
}
