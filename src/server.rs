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

use std::env::current_dir;
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

/// What the startup notice names besides the version: the directory every relative
/// path resolves in, the shell used to execute a command, the read size that applies
/// to a sender stating no limit, and whether `edit` accepts an `[upto]` marker.  The
/// directory is left out rather than losing the hello if the process cannot say where
/// it is.
fn startup_details(config: &Config) -> String {
    let mut details = Vec::new();
    if let Ok(dir) = current_dir() {
        details.push(format!("dir {}", dir.display()));
    }
    details.push(format!("shell {}", config.shell.display()));
    details.push(format!("read_lines default {}", config.read_lines));
    details.push(format!(
        "upto marker {}",
        if config.edit_upto { "on" } else { "off" }
    ));
    details.join(", ")
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
    //
    // The first one is the startup handshake, written before anything is read back:
    // the agent blocks on the word `ready` rather than loading a model for a sandbox
    // that does not exist, echoes this helper's stderr until the notice arrives, and
    // then prints this line as the sandbox's hello.  The word is the contract; the
    // rest is for whoever is watching the run start.
    let startup = notice(&format!(
        "ds4-sandbox-helper {} ready: {}",
        env!("CARGO_PKG_VERSION"),
        startup_details(&config)
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
