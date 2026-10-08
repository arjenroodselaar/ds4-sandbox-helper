// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The request loop is one frame in, one frame out, in order.
//!
//! The agent keeps one request in flight, which is what lets this be a loop rather
//! than a multiplexer.  The concurrency a long command needs lives inside the tool.
//!
//! Three things end the loop: stdin reaching EOF, a stream that has stopped making
//! sense, and the agent's teardown signal.  All of them leave through the same session
//! teardown, because the commands a session started are its own to stop.
//!
//! Anything unreadable ends the session instead of being worked around.  Byte counts
//! that stopped agreeing with the peer cannot be answered without guessing.

use std::env::current_dir;
use std::io::ErrorKind;

use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::BufReader;
use tokio::signal::unix::Signal;
use tokio::signal::unix::SignalKind;
use tokio::signal::unix::signal;

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

/// Why the loop stopped.  The exit code and the stderr line come from this.
pub enum Outcome {
    /// The agent closed the pipe.
    Finished,
    /// The peer stopped making sense.
    Fault(String),
}

/// What the startup notice names besides the version.  A directory the process cannot
/// name is left out rather than losing the notice.
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

    // The startup handshake, written before anything is read back.  The agent blocks
    // on the word `ready` before it loads a model.  Later id-0 notices are
    // diagnostics, mirrored into the agent's sandbox log.
    let startup = notice(&format!(
        "ds4-sandbox-helper {} ready: {}",
        env!("CARGO_PKG_VERSION"),
        startup_details(&config)
    ));
    if write(&mut writer, &startup).await.is_err() {
        session.finish().await;
        return Outcome::Finished;
    }

    // The agent's teardown closes stdin and then escalates SIGTERM to SIGKILL for this
    // process's group.  The commands started here run in groups of their own, which
    // that signal does not reach, so dying on it would leave them running behind the
    // helper.  Answering it is what gives the teardown a chance to stop them.  A
    // helper that cannot catch it keeps the default disposition, and the agent's
    // SIGKILL stays the backstop.
    let mut terminate = signal(SignalKind::terminate()).ok();

    loop {
        let received = tokio::select! {
            _ = termination(&mut terminate) => {
                session.finish().await;
                return Outcome::Finished;
            }
            received = wire::read_frame(&mut reader, MAX_REQUEST_BYTES) => received,
        };
        let frame = match received {
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
                // No id to answer to, so say what happened and stay byte-aligned.
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
        // A frame over the limit would be refused outright.  An error keeps the session.
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
            // The agent exits when the run ends, so a last answer can meet EPIPE.
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

/// Waits for the agent's teardown signal.  There is nothing to read out of it, since
/// the arrival is the message, and the session ends the way it ends at a closed stdin.
///
/// A `None` means the signal could not be caught, which leaves the default disposition
/// alone.  This never wakes then, and the closed stdin the agent signals after is what
/// ends the session, as it would anyway.
async fn termination(term: &mut Option<Signal>) {
    match term {
        Some(signal) => {
            signal.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// The start of an unparsable request, quoted in the exit message.
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
