// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The request loop is one frame in, one frame out, in order.
//!
//! The agent keeps one request in flight, which is what lets this be a loop rather
//! than a multiplexer.  The concurrency a long command needs lives inside the tool.
//!
//! Three things end the loop: stdin reaching EOF, a stream that has stopped making
//! sense, and a termination signal, which is SIGTERM from the agent and SIGINT from a
//! terminal.  All of them leave through the same session teardown, because the commands
//! a session started are its own to stop.
//!
//! A signal that arrives while a request is in flight does not drop it.  The docs give
//! the agent one second between SIGTERM and SIGKILL, and a request abandoned mid-`bash`
//! could leave a command between being spawned and being recorded in the job list,
//! which is the one window no teardown can close.  So the request is told to stop
//! waiting, answers, and the loop ends on its way back around.
//!
//! Anything unreadable ends the session instead of being worked around.  Byte counts
//! that stopped agreeing with the peer cannot be answered without guessing.

use std::env::current_dir;
use std::future::Future;
use std::io::ErrorKind;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::BufReader;
use tokio::signal::unix::Signal;
use tokio::signal::unix::SignalKind;
use tokio::signal::unix::signal;

use crate::protocol;
use crate::protocol::Request;
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
    if wire::write_frame(&mut writer, &startup).await.is_err() {
        session.finish().await;
        return Outcome::Finished;
    }

    // The agent's teardown closes stdin and then escalates SIGTERM to SIGKILL for this
    // process's group.  The commands started here run in groups of their own, which
    // that signal does not reach, so dying on it would leave them running behind the
    // helper.  Answering it is what gives the teardown a chance to stop them.  A
    // helper that cannot catch it keeps the default disposition, and the agent's
    // SIGKILL stays the backstop.
    let ending = session.ending.clone();

    // The signals are watched by a task of their own, whose only work is to set the
    // notice.  Waiting for them in the loop below would mean the notice exists only
    // once the loop is back around to poll it, and a signal that lands while a tool is
    // waiting would go unnoticed until that tool ran out on its own.  SIGINT is watched
    // for the same reason as SIGTERM: a Ctrl-C at a terminal reaches this process too,
    // and its default disposition would stop the helper where it stands, with the job
    // groups the docs promise to stop still running.
    {
        let ending = ending.clone();
        tokio::spawn(async move {
            let mut terminate = signal(SignalKind::terminate()).ok();
            let mut interrupt = signal(SignalKind::interrupt()).ok();
            tokio::select! {
                _ = signalled(&mut terminate) => {}
                _ = signalled(&mut interrupt) => {}
            }
            ending.trigger();
        });
    }

    loop {
        // A notice already set resolves this at once, which is what ends the session
        // when the signal landed during a request rather than between two.
        let received = tokio::select! {
            _ = ending.wait() => {
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
                let _ = wire::write_frame(
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

        let reply = match run_guarded(&request, &mut session, &config).await {
            Ok(Ok(result)) => response_ok(request.id, &result),
            Ok(Err(message)) => response_error(request.id, &message),
            Err(()) => {
                // A tool that panicked may have left the session half updated, and
                // state like that is not worth trusting with the next request.  The
                // answer still goes out first: losing the call is survivable, losing
                // both the call and the teardown is what happens today.
                let reply = response_error(request.id, "the tool panicked, and the session ended");
                let _ = wire::write_frame(&mut writer, &reply).await;
                session.finish().await;
                return Outcome::Fault(format!("the tool panicked in request {}", request.id));
            }
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

        if let Err(err) = wire::write_frame(&mut writer, &reply).await {
            session.finish().await;
            // The agent exits when the run ends, so a last answer can meet EPIPE.
            return if err.kind() == ErrorKind::BrokenPipe {
                Outcome::Finished
            } else {
                Outcome::Fault(format!("writing a response: {err}"))
            };
        }

        // The signal arrived while that request was working, and its answer is out.
        // Waiting for the next frame instead would spend the agent's whole grace on a
        // read that a tool has already been told to give up on.
        if ending.triggered() {
            session.finish().await;
            return Outcome::Finished;
        }
    }
}

/// One request, with a panic boundary around it.
///
/// A panic in a tool unwinds out of the runtime and takes the session with it, which
/// loses the answer the agent is waiting for.  Catching it at the poll boundary is
/// what costs no ownership change: `spawn` would need the session moved behind a task,
/// and that is what lets a request be dropped between spawning a command and recording
/// it in the job list.
async fn run_guarded(
    request: &Request,
    session: &mut Session,
    config: &Config,
) -> Result<Result<String, String>, ()> {
    Guarded {
        inner: Box::pin(tools::run(request, session, config)),
    }
    .await
}

/// A future whose panics come out as `Err(())` instead of unwinding the caller.
struct Guarded<F> {
    inner: Pin<Box<F>>,
}

impl<F: Future> Future for Guarded<F> {
    type Output = Result<F::Output, ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `UnwindSafe` is asserted, not proven: the future is dropped here rather than
        // polled again, and the caller ends the session instead of reusing it.
        match catch_unwind(AssertUnwindSafe(|| self.get_mut().inner.as_mut().poll(cx))) {
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(())),
        }
    }
}

/// Waits for the agent's teardown signal.  There is nothing to read out of it, since
/// the arrival is the message, and the session ends the way it ends at a closed stdin.
///
/// A `None` means the signal could not be caught, which leaves the default disposition
/// alone.  This never wakes then, and the closed stdin the agent signals after is what
/// ends the session, as it would anyway.
async fn signalled(term: &mut Option<Signal>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_request_that_finishes_comes_back_unchanged() {
        let done = Guarded {
            inner: Box::pin(async { 7 }),
        };
        assert_eq!(done.await, Ok(7));
    }

    /// The point of the boundary.  A panic inside a tool stops that call, and not the
    /// session that was asked to answer it.
    #[tokio::test]
    async fn a_request_that_panics_comes_back_as_a_fault() {
        let loud = Guarded {
            inner: Box::pin(async { panic!("the tool fell over") }),
        };
        assert_eq!(loud.await, Err(()));
    }
}
