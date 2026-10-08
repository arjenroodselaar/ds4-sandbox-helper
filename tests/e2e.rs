// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests run the real binary, with real pipes, real files and real processes.
//!
//! These drive the helper exactly as `ds4-agent --sandbox` does — framed JSON both
//! ways — so they check what unit tests cannot: that byte counts agree with payloads,
//! that every request gets a reply, and that a tool's answer keeps its shape through
//! the protocol.  The codec is written again here on purpose.  A test that imported the
//! binary's own encoder could not catch an encoder that disagrees with the spec.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;

struct Helper {
    child: Child,
    stdin: ChildStdin,
    stdout: Buf,
    next_id: i64,
}

/// stdout with just enough buffering to read a header and then an exact byte count.
struct Buf {
    stream: ChildStdout,
    pending: Vec<u8>,
}

impl Buf {
    async fn byte(&mut self) -> u8 {
        if self.pending.is_empty() {
            self.pending.resize(1, 0);
            let read = self.stream.read(&mut self.pending).await.expect("read");
            assert!(read > 0, "the helper closed its stdout");
        }
        let byte = self.pending[0];
        self.pending.remove(0);
        byte
    }

    async fn exactly(&mut self, count: usize) -> Vec<u8> {
        let mut out = vec![0u8; count];
        for slot in out.iter_mut() {
            *slot = self.byte().await;
        }
        out
    }

    async fn line(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = self.byte().await;
            if byte == b'\n' {
                return out;
            }
            out.push(byte);
        }
    }
}

impl Helper {
    async fn start(args: &[&str]) -> Helper {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ds4-sandbox-helper"))
            .args(args)
            // Which shell a run with no option uses is a question about the sandbox,
            // not about whoever exported DS4_SHELL.
            .env_remove("DS4_SHELL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Diagnostics stay on the inherited stderr.  The agent's contract is that
            // nothing but frames ever reaches stdout.
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the helper");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = Buf {
            stream: child.stdout.take().expect("stdout"),
            pending: Vec::new(),
        };
        let mut helper = Helper {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        // The startup notice has to arrive before a byte is written.  The agent blocks
        // on the word ready, so a hello that waited for a request would never start.
        let first = helper.one().await;
        assert_eq!(
            first["id"].as_i64(),
            Some(0),
            "expected a notice first, got {first}"
        );
        assert_eq!(first["type"], "log", "expected a notice first, got {first}");
        let hello = first["text"].as_str().unwrap_or_default();
        assert!(
            hello.to_ascii_lowercase().contains("ready"),
            "the startup notice has to contain the word ready: {hello}"
        );
        // The notice is also where a run says which shell it executes with.
        assert!(
            hello.contains("shell "),
            "the notice names no shell: {hello}"
        );
        helper
    }

    /// Reads the next frame, whatever it is.
    async fn one(&mut self) -> serde_json::Value {
        let header = self.stdout.line().await;
        let count: usize = String::from_utf8_lossy(&header)
            .parse()
            .unwrap_or_else(|_| {
                panic!(
                    "header {:?} is not a byte count",
                    String::from_utf8_lossy(&header)
                )
            });
        let payload = self.stdout.exactly(count).await;
        serde_json::from_slice(&payload).unwrap_or_else(|err| panic!("frame is not JSON: {err}"))
    }

    /// Reads the next frame that answers something, skipping anything the helper
    /// volunteers on its own, which the agent also treats as a notice.
    async fn frame(&mut self) -> serde_json::Value {
        loop {
            let value = self.one().await;
            if value["id"].as_i64() == Some(0) && value["type"] == "log" {
                continue;
            }
            return value;
        }
    }

    /// Sends a request and returns the reply.  Every argument is a string, which is
    /// what the agent sends, numbers and booleans included.
    async fn call(&mut self, tool: &str, args: serde_json::Value) -> serde_json::Value {
        self.request(serde_json::json!({ "tool": tool, "args": args }))
            .await
    }

    /// Sends a request frame carrying more than a tool and its arguments.  The agent
    /// puts what a tool cannot work out for itself (the model's context size, say) at
    /// the top level, beside `id` rather than inside `args`.
    async fn request(&mut self, mut frame: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id;
        frame["id"] = serde_json::json!(id);
        let request = frame;
        let payload = request.to_string();
        self.stdin
            .write_all(format!("{}\n{payload}", payload.len()).as_bytes())
            .await
            .expect("write request");
        self.stdin.flush().await.expect("flush");
        let reply = self.frame().await;
        assert_eq!(
            reply["id"].as_i64(),
            Some(id),
            "answer to the wrong request"
        );
        reply
    }

    async fn ok(&mut self, tool: &str, args: serde_json::Value) -> String {
        let reply = self.call(tool, args).await;
        assert!(
            reply["ok"].as_bool() == Some(true),
            "{tool} failed: {}",
            reply["error"]
        );
        reply["result"]
            .as_str()
            .expect("a result string")
            .to_string()
    }

    async fn fail(&mut self, tool: &str, args: serde_json::Value) -> String {
        let reply = self.call(tool, args).await;
        assert!(
            reply["ok"].as_bool() == Some(false),
            "{tool} unexpectedly succeeded: {}",
            reply["result"]
        );
        reply["error"]
            .as_str()
            .expect("an error string")
            .to_string()
    }

    /// Closes stdin and waits.  The helper should treat that as the end of the run.
    async fn finish(self) -> std::process::ExitStatus {
        let Helper {
            mut child,
            stdin,
            stdout,
            ..
        } = self;
        // stdout has to go before stdin.  Some systems wake a blocked reader before
        // delivering EOF the other way, and the helper would keep reading frames.
        drop(stdout);
        drop(stdin);
        child.wait().await.expect("wait for the helper")
    }
}

/// A directory for one test to make its mess in.  The caller keeps it alive.
fn scratch(tag: &str) -> (std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::TempDir::with_prefix(format!("ds4-helper-e2e-{tag}-"))
        .expect("scratch directory");
    (dir.path().to_path_buf(), dir)
}

#[tokio::test]
async fn a_write_and_a_read_round_trip_through_the_wire() {
    let (dir, _dir) = scratch("write");
    let file = dir.join("notes.txt");
    let mut helper = Helper::start(&[]).await;

    let text = helper
        .ok(
            "write",
            serde_json::json!({"path": file.to_str().unwrap(), "content": "alpha\nbravo\n"}),
        )
        .await;
    assert!(text.starts_with("Wrote 12 bytes to "), "{text}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\nbravo\n");

    let read = helper
        .ok("read", serde_json::json!({"path": file.to_str().unwrap()}))
        .await;
    assert!(
        read.contains(&format!("{}: lines 1-2 (end of file)\n", file.display())),
        "{read}"
    );
    assert!(read.contains("1 alpha\n"), "{read}");

    let missing = helper
        .fail("read", serde_json::json!({"path": "/no/such/file"}))
        .await;
    assert!(missing.starts_with("read failed: "), "{missing}");

    helper.finish().await;
}

#[tokio::test]
async fn more_resumes_a_long_file_where_read_stopped() {
    let (dir, _dir) = scratch("more");
    let file = dir.join("big.txt");
    std::fs::write(
        &file,
        (1..=300)
            .map(|n| format!("row {n} of three hundred\n"))
            .collect::<String>(),
    )
    .unwrap();

    let mut helper = Helper::start(&["--read-lines", "3"]).await;
    let first = helper
        .ok("read", serde_json::json!({"path": file.to_str().unwrap()}))
        .await;
    assert!(first.contains("lines 1-3 (partial read)"), "{first}");
    assert!(first.contains("3 row 3 of three hundred\n"), "{first}");
    assert!(first.contains("continue_offset=4"), "{first}");

    let second = helper.ok("more", serde_json::json!({"count": "2"})).await;
    assert!(second.contains("lines 4-5 (partial read)"), "{second}");
    assert!(second.contains("4 row 4 of three hundred\n"), "{second}");

    // A read that reaches the end drops the resume state, so a later more says so
    // rather than repeating the last chunk forever.
    helper
        .ok(
            "read",
            serde_json::json!({"path": file.to_str().unwrap(), "start_line": "299"}),
        )
        .await;
    let done = helper.fail("more", serde_json::json!({})).await;
    assert_eq!(done, "no previous output to continue");

    helper.finish().await;
}

#[tokio::test]
async fn an_edit_reports_the_span_it_touched() {
    let (dir, _dir) = scratch("edit");
    let file = dir.join("main.c");
    std::fs::write(&file, "static int one(void) { return 1; }\n").unwrap();

    let mut helper = Helper::start(&[]).await;
    let text = helper
        .ok(
            "edit",
            serde_json::json!({"path": file.to_str().unwrap(), "old": "return 1;", "new": "return 2;"}),
        )
        .await;
    assert!(text.contains("using old/new replacement"), "{text}");
    assert!(text.contains("Touched old lines 1-1"), "{text}");
    assert!(text.contains("Current file around edit: "), "{text}");
    assert!(
        text.contains("1 static int one(void) { return 2; }"),
        "{text}"
    );
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("return 2;")
    );

    let ambiguous = helper
        .fail(
            "edit",
            serde_json::json!({"path": file.to_str().unwrap(), "old": "o", "new": "0"}),
        )
        .await;
    // "o" appears in "one", "void" and "return", so the selector names more
    // than one place and the file has to stay exactly as it was.
    assert_eq!(ambiguous, "old text anchor is not unique");
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("return 2;"),
        "a refused edit must leave the file alone"
    );

    helper.finish().await;
}

#[tokio::test]
async fn list_and_search_answer_over_the_same_channel() {
    let (dir, _dir) = scratch("tree");
    std::fs::write(dir.join("a.c"), "int main(void) { return 0; }\n").unwrap();
    std::fs::write(dir.join("b.c"), "int add(int a, int b) { return a + b; }\n").unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/c.c"), "int sub(void) { return 3; }\n").unwrap();

    let mut helper = Helper::start(&[]).await;

    let listing = helper
        .ok("list", serde_json::json!({"path": dir.to_str().unwrap()}))
        .await;
    assert!(
        listing.starts_with(&format!("{}:\n", dir.display())),
        "{listing}"
    );
    assert!(listing.contains("sub/\n"), "{listing}");
    assert!(listing.contains("a.c\n"), "{listing}");

    let found = helper
        .ok(
            "search",
            serde_json::json!({"query": "int", "path": dir.to_str().unwrap(), "context": "0"}),
        )
        .await;
    assert!(found.starts_with("3 matches shown\n\n"), "{found}");
    assert!(
        found.contains("  1 int main(void) { return 0; }"),
        "{found}"
    );
    assert!(
        found.contains(&format!("{}/sub/c.c\n", dir.display())),
        "{found}"
    );

    let regex = helper
        .ok(
            "search",
            serde_json::json!({"query": "add|sub", "path": dir.to_str().unwrap(), "mode": "regex", "glob": "*.c"}),
        )
        .await;
    assert!(regex.contains("int add("), "{regex}");
    assert!(regex.contains("int sub("), "{regex}");
    assert!(!regex.contains("int main"), "{regex}");

    let nothing = helper
        .ok(
            "search",
            serde_json::json!({"query": "nothing here at all", "path": dir.to_str().unwrap()}),
        )
        .await;
    assert_eq!(nothing, "No matches in searched text\n");

    helper.finish().await;
}

#[tokio::test]
async fn a_fast_command_comes_back_with_its_output() {
    let mut helper = Helper::start(&[]).await;
    let text = helper
        .ok(
            "bash",
            serde_json::json!({"command": "printf 'one\\ntwo\\n'"}),
        )
        .await;
    assert!(text.contains("status=done"), "{text}");
    assert!(text.contains("exit_status=0\n"), "{text}");
    assert!(text.contains("<output>\none\ntwo\n</output>"), "{text}");

    let failed = helper
        .ok("bash", serde_json::json!({"command": "exit 3"}))
        .await;
    assert!(failed.contains("exit_status=3"), "{failed}");

    // Output the command never printed is still reported as an empty block, so the
    // model can tell "no output" from "output we could not read".
    let quiet = helper
        .ok("bash", serde_json::json!({"command": "true"}))
        .await;
    assert!(quiet.contains("<output>\n</output>"), "{quiet}");

    helper.finish().await;
}

#[tokio::test]
async fn a_slow_command_keeps_running_until_it_is_stopped() {
    let mut helper = Helper::start(&[]).await;

    // A job that outlasts the shortest wait the tool allows, so the first snapshot
    // comes back while it is still running — which is how a long build is started.
    // (`refresh_sec` is clamped to a second, as it is by the agent.)
    let started = helper
        .ok(
            "bash",
            serde_json::json!({"command": "echo first; sleep 30; echo last", "refresh_sec": "0"}),
        )
        .await;
    assert!(started.contains("status=running"), "{started}");
    assert!(started.contains("Use bash_status job="), "{started}");
    let job = started
        .split("job=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("a job number")
        .to_string();
    let spool = started
        .lines()
        .find_map(|line| line.strip_prefix("output_path="))
        .and_then(|rest| rest.split(" (").next())
        .expect("an output path")
        .to_string();
    assert!(std::path::Path::new(&spool).exists(), "spool file gone");
    // Named the way the agent names its own command output, so both logs read alike.
    assert!(
        std::path::Path::new(&spool)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("ds4_agent_output_"),
        "{spool}"
    );

    let later = helper
        .ok("bash_status", serde_json::json!({"job": job.as_str()}))
        .await;
    assert!(later.contains("status=running"), "{later}");
    // A second look shows the tail, not the head again.
    assert!(later.contains("<tail -4 "), "{later}");

    let stopped = helper
        .ok("bash_stop", serde_json::json!({"job": job.as_str()}))
        .await;
    assert!(stopped.contains("status=done"), "{stopped}");
    assert!(
        stopped.contains("exit_status=143") || stopped.contains("exit_status=137"),
        "{stopped}"
    );

    // The job and its spool file are gone once the model has been shown the end.
    let gone = helper
        .fail("bash_status", serde_json::json!({"job": job.as_str()}))
        .await;
    assert!(gone.starts_with("bash job not found"), "{gone}");
    assert!(!std::path::Path::new(&spool).exists(), "spool file left");

    helper.finish().await;
}

/// What `refresh_sec` means on a stop.  It is how long the helper may take, not how
/// long it takes.  A command that ignores `SIGTERM` is killed by the follow-up and
/// answered when it is reaped, not at the deadline.
#[tokio::test]
async fn the_wait_a_stop_asks_for_is_a_ceiling_not_a_delay() {
    let mut helper = Helper::start(&[]).await;
    let started = helper
        .ok(
            "bash",
            serde_json::json!({"command": "trap '' TERM; sleep 30", "refresh_sec": "0"}),
        )
        .await;
    assert!(started.contains("status=running"), "{started}");
    let job = started
        .split_whitespace()
        .find_map(|word| word.strip_prefix("job="))
        .expect("a job handle")
        .to_string();

    let since = std::time::Instant::now();
    let stopped = helper
        .request(serde_json::json!({
            "tool": "bash_stop", "args": {"job": job.as_str()}, "refresh_sec": "20"
        }))
        .await;
    let took = since.elapsed();
    let text = stopped["result"].as_str().unwrap_or_default().to_string();

    assert_eq!(stopped["ok"], true, "{stopped}");
    assert!(text.contains("status=done"), "{text}");
    // Ignored SIGTERM, so the signal that could not be ignored finished it.
    assert!(text.contains("exit_status=137"), "{text}");
    assert!(
        took < std::time::Duration::from_secs(10),
        "the stop waited {took:?} for a job that was already dead"
    );

    helper.finish().await;
}

#[tokio::test]
async fn a_command_that_ignores_its_deadline_is_killed() {
    let mut helper = Helper::start(&[]).await;
    let text = helper
        .ok(
            "bash",
            serde_json::json!({"command": "sleep 60", "timeout_sec": "1", "refresh_sec": "8"}),
        )
        .await;
    assert!(text.contains("timed_out=1"), "{text}");
    // Killed rather than exited, and reported the way a shell reports it.
    assert!(
        text.contains("exit_status=143") || text.contains("exit_status=137"),
        "{text}"
    );
    helper.finish().await;
}

#[tokio::test]
async fn a_shell_cannot_read_the_request_stream() {
    // The helper's stdin is the agent's frame stream.  A command that inherited it
    // would eat the next request and desynchronise the session.
    let mut helper = Helper::start(&[]).await;
    let text = helper
        .ok(
            "bash",
            serde_json::json!({"command": "wc -c < /dev/stdin 2>/dev/null || echo unreadable"}),
        )
        .await;
    assert!(
        text.contains("0\n") || text.contains("unreadable"),
        "the command read something: {text}"
    );

    // And the following request still answers, which is the part that matters.
    let listing = helper.ok("list", serde_json::json!({"path": "."})).await;
    assert!(listing.starts_with(".:\n"), "{listing}");
    helper.finish().await;
}

/// Which shell executes a command is settled when the helper starts.  `$0` is the
/// cheapest way to ask a shell which shell it is, because `$0` is the name it was
/// started with.
#[tokio::test]
async fn a_command_is_executed_with_bash_when_nothing_is_chosen() {
    let expected = if Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    let mut helper = Helper::start(&[]).await;
    let answer = helper
        .ok("bash", serde_json::json!({"command": "echo $0"}))
        .await;
    assert!(
        answer.contains(&format!("<output>\n{expected}\n</output>")),
        "{answer}"
    );
    helper.finish().await;
}

#[tokio::test]
async fn a_shell_named_without_a_slash_is_resolved_by_the_exec_call() {
    let mut helper = Helper::start(&["--shell", "sh"]).await;
    let answer = helper
        .ok("bash", serde_json::json!({"command": "echo $0"}))
        .await;
    // The name asked for rather than the one PATH found.  That is proof the shell came
    // from the option.
    assert!(answer.contains("<output>\nsh\n</output>"), "{answer}");
    helper.finish().await;
}

/// A script that is not a shell at all is the clearest proof of the choice.  It answers
/// with what it was handed, and only the shell named at startup hands it over that way.
#[tokio::test]
async fn the_shell_named_at_startup_is_the_one_used_to_execute_a_command() {
    let (dir, _dir) = scratch("shell");
    let fake = dir.join("fake-shell");
    std::fs::write(&fake, "#!/bin/sh\necho \"chosen shell got: $2\"\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut helper = Helper::start(&["--shell", fake.to_str().unwrap()]).await;
    let answer = helper
        .ok("bash", serde_json::json!({"command": "echo hi"}))
        .await;
    // `$1` is the `-c` the helper adds and `$2` the command after it, so the shell is
    // addressed the way every shell is, and the model sees neither.
    assert!(answer.contains("chosen shell got: echo hi"), "{answer}");
    helper.finish().await;
}

#[tokio::test]
async fn a_shell_that_cannot_be_run_stops_before_the_first_frame() {
    let child = Command::new(env!("CARGO_BIN_EXE_ds4-sandbox-helper"))
        .args(["--shell", "/definitely/not/here"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the helper");
    let output = child.wait_with_output().await.expect("wait");
    assert_eq!(output.status.code(), Some(1), "{:?}", output.status);
    assert!(
        output.stdout.is_empty(),
        "a refused --shell printed frames: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        text.contains("invalid shell /definitely/not/here"),
        "{text}"
    );
}

#[tokio::test]
async fn a_request_for_an_unknown_tool_is_answered_not_ignored() {
    let mut helper = Helper::start(&[]).await;
    let error = helper
        .fail("view_image", serde_json::json!({"path": "x"}))
        .await;
    assert_eq!(error, "unknown tool: view_image");
    // The session survives it.  An error is an answer, not a fault.
    let ok = helper
        .ok("bash", serde_json::json!({"command": "true"}))
        .await;
    assert!(ok.contains("exit_status=0"), "{ok}");
    helper.finish().await;
}

#[tokio::test]
async fn garbage_on_stdin_ends_the_session_with_a_reason() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ds4-sandbox-helper"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the helper");
    let mut stdin = child.stdin.take().unwrap();
    // Not a byte count at all.  There is no way to know where the next frame starts.
    stdin.write_all(b"this is not a frame\n").await.unwrap();
    stdin.flush().await.unwrap();
    let status = child.wait().await.expect("wait");
    assert!(!status.success(), "a garbage frame must not exit clean");
}

/// A frame counted at more bytes than the helper accepts is still a frame it has to
/// count out, and a peer that stops in the middle of that has not ended the session.
/// Counting a payload out has no buffer that can come up short, which is how this used
/// to read as a clean close.
#[tokio::test]
async fn an_oversized_frame_that_stops_short_ends_the_session_with_a_reason() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ds4-sandbox-helper"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the helper");
    let mut stdin = child.stdin.take().unwrap();
    // Over the request limit, so the helper counts this one out rather than parsing it,
    // and far short of the count it was given.
    stdin.write_all(b"2000013\nfour").await.unwrap();
    stdin.flush().await.unwrap();
    // Dropping the write end is the EOF in the middle of those counted bytes.
    drop(stdin);
    let output = child.wait_with_output().await.expect("wait");
    assert_eq!(output.status.code(), Some(1), "{:?}", output.status);
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(text.contains("stream ended after"), "{text}");
    assert!(text.contains("of 2000013 payload bytes"), "{text}");
}

#[tokio::test]
async fn a_frame_that_is_not_a_json_object_ends_the_session() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ds4-sandbox-helper"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the helper");
    let mut stdin = child.stdin.take().unwrap();
    let payload = b"[1,2,3]";
    stdin
        .write_all(format!("{}\n", payload.len()).into_bytes().as_slice())
        .await
        .unwrap();
    stdin.write_all(payload).await.unwrap();
    stdin.flush().await.unwrap();
    let status = child.wait().await.expect("wait");
    assert!(
        !status.success(),
        "an unanswerable frame must not exit clean"
    );
}

/// An id with a fraction is refused rather than rounded down, because answering it as
/// the whole number would complete a request the agent never sent and would then be
/// dropped by the agent as an answer to nothing.
#[tokio::test]
async fn a_frame_whose_id_is_not_an_integer_ends_the_session() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ds4-sandbox-helper"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the helper");
    let mut stdin = child.stdin.take().unwrap();
    let payload = br#"{"id":7.9,"tool":"list"}"#;
    stdin
        .write_all(format!("{}\n", payload.len()).into_bytes().as_slice())
        .await
        .unwrap();
    stdin.write_all(payload).await.unwrap();
    stdin.flush().await.unwrap();
    let status = child.wait().await.expect("wait");
    assert!(
        !status.success(),
        "a request that cannot be answered by id must not exit clean"
    );
}

/// `--chdir` is the only thing that says what a relative path means to this helper,
/// so it has to hold for every tool that is given one.
#[tokio::test]
async fn a_relative_path_is_answered_from_the_directory_asked_for_at_start() {
    let (dir, _dir) = scratch("chdir");
    let mut helper = Helper::start(&["--chdir", dir.to_str().unwrap()]).await;

    let answer = helper
        .ok(
            "write",
            serde_json::json!({"path": "inside.txt", "content": "one\n"}),
        )
        .await;
    assert!(answer.contains("Wrote 4 bytes"), "{answer}");
    assert_eq!(std::fs::read(dir.join("inside.txt")).unwrap(), b"one\n");

    let answer = helper
        .ok("read", serde_json::json!({"path": "inside.txt"}))
        .await;
    assert!(answer.contains("one"), "{answer}");

    // A shell started by `bash` begins in the same place, so no command has to name
    // the root of the session.
    let answer = helper
        .ok("bash", serde_json::json!({"command": "cat inside.txt"}))
        .await;
    assert!(answer.contains("one"), "{answer}");

    // An absolute path still means what it says.  The directory is where relative ones
    // start, not a wall around the session.
    let answer = helper
        .ok("read", serde_json::json!({"path": "/etc/passwd"}))
        .await;
    assert!(!answer.trim().is_empty(), "{answer}");
    helper.finish().await;
}

/// A directory that cannot be worked in is a mistake in the command line.  The helper
/// leaves before reading a frame, saying the agent's own complaint on stderr.
#[tokio::test]
async fn a_chdir_that_cannot_be_done_stops_before_the_first_frame() {
    let child = Command::new(env!("CARGO_BIN_EXE_ds4-sandbox-helper"))
        .args(["--chdir", "/definitely/not/here"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the helper");
    let output = child.wait_with_output().await.expect("wait");
    assert_eq!(output.status.code(), Some(1), "{:?}", output.status);
    assert!(
        output.stdout.is_empty(),
        "a refused --chdir printed frames: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        text.contains("invalid working directory /definitely/not/here"),
        "{text}"
    );
}

#[tokio::test]
async fn closing_stdin_ends_the_run_cleanly() {
    let mut helper = Helper::start(&[]).await;
    helper
        .ok("bash", serde_json::json!({"command": "true"}))
        .await;
    let status = helper.finish().await;
    assert!(status.success(), "an ordinary end is not a failure");
}

#[tokio::test]
async fn a_huge_answer_is_cut_to_the_tool_limit_and_says_so() {
    let (dir, _dir) = scratch("limit");
    let file = dir.join("long.txt");
    std::fs::write(&file, "x".repeat(2_000_000)).unwrap();

    let mut helper = Helper::start(&[]).await;
    let text = helper
        .ok(
            "read",
            serde_json::json!({"path": file.to_str().unwrap(), "max_lines": "1"}),
        )
        .await;
    assert!(text.len() <= 128 * 1024, "{} bytes", text.len());
    assert!(
        text.contains("[Read truncated."),
        "{tail}",
        tail = &text[text.len() - 200..]
    );

    let whole = helper
        .fail(
            "read",
            serde_json::json!({"path": file.to_str().unwrap(), "whole": "true"}),
        )
        .await;
    assert!(whole.contains("whole read exceeds"), "{whole}");

    helper.finish().await;
}

#[tokio::test]
async fn the_read_size_that_comes_with_the_request_wins() {
    let (dir, _dir) = scratch("readlines");
    let file = dir.join("numbers.txt");
    std::fs::write(
        &file,
        (1..=1000)
            .map(|n| format!("line {n}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let path = file.to_str().unwrap().to_string();

    // Started with a small default, because a helper does not know the model.
    let mut helper = Helper::start(&["--read-lines", "3"]).await;

    let told = helper
        .request(serde_json::json!({"tool":"read","limits":{"read_lines":7},"args":{"path":path}}))
        .await;
    let text = told["result"].as_str().unwrap_or_default().to_string();
    assert!(text.contains("lines 1-7 (partial read)"), "{text}");

    // `more` takes its size from the same place, so a resumed read does not change
    // width mid-file.  `max_bytes` is a cap this helper does not implement, so it is
    // ignored.
    let resumed = helper
        .request(
            serde_json::json!({"tool":"more","limits":{"read_lines":2,"max_bytes":100},"args":{}}),
        )
        .await;
    let text = resumed["result"].as_str().unwrap_or_default().to_string();
    assert!(text.contains("lines 8-9 (partial read)"), "{text}");

    // Without the number, the helper's own setting is what a bare read means.
    let untold = helper
        .ok("read", serde_json::json!({"path": path.as_str()}))
        .await;
    assert!(untold.contains("lines 1-3 (partial read)"), "{untold}");

    // The size is capped.  A frame may ask for a lot, but not for everything.  The
    // ceiling is written out here because a test of a binary cannot import it.
    const CAP: usize = 500;
    let greedy = helper
        .request(
            serde_json::json!({"tool":"read","limits":{"read_lines":50000},"args":{"path":path}}),
        )
        .await;
    let text = greedy["result"].as_str().unwrap_or_default().to_string();
    assert!(
        text.contains(&format!("lines 1-{CAP} (partial read)")),
        "{}",
        &text[..200.min(text.len())]
    );
    assert!(
        text.contains(&format!("{CAP} line {CAP}\n")),
        "{}",
        &text[text.len().saturating_sub(400)..]
    );

    // A number that is not a number is treated as absent, not as an error.  The model
    // does not write this field, and a broken sender should still get read.
    let junk = helper
        .request(
            serde_json::json!({"tool":"read","limits":{"read_lines":"lots"},"args":{"path":path}}),
        )
        .await;
    let text = junk["result"].as_str().unwrap_or_default().to_string();
    assert!(text.contains("lines 1-3 (partial read)"), "{text}");

    helper.finish().await;
}

/// The size of a job's output is read from the spool file here, where the C agent counts
/// the bytes it wrote itself, so a file that cannot be read has to be said out loud
/// rather than answered as an empty one.
#[tokio::test]
async fn output_that_cannot_be_read_back_is_not_reported_as_empty() {
    let mut helper = Helper::start(&[]).await;
    let started = helper
        .ok(
            "bash",
            serde_json::json!({"command": "echo out; sleep 30", "refresh_sec": "0"}),
        )
        .await;
    let job = started
        .split("job=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("a job number");
    let spool = started
        .lines()
        .find_map(|line| line.strip_prefix("output_path="))
        .and_then(|rest| rest.split(" (").next())
        .expect("an output path");

    // The model is shown the path and can do what it likes with it.
    let removed = helper
        .ok(
            "bash",
            serde_json::json!({"command": format!("rm -f {spool}")}),
        )
        .await;
    assert!(removed.contains("status=done"), "{removed}");

    let later = helper
        .ok("bash_status", serde_json::json!({"job": job}))
        .await;
    assert!(later.contains("status=running"), "{later}");
    assert!(
        later.contains("Tool error: command output could not be read: No such file"),
        "{later}"
    );
    assert!(!later.contains("<output>"), "{later}");
    assert!(!later.contains("0 bytes"), "{later}");

    let stopped = helper
        .ok("bash_stop", serde_json::json!({"job": job}))
        .await;
    assert!(stopped.contains("status=done"), "{stopped}");

    helper.finish().await;
}

// --------------------------------------------------------------------------------
// Session teardown.  A job runs in a process group of its own, which is what keeps a
// runaway command from being able to hurt the helper, and what means the agent's
// teardown signal stops at the helper's group.  Stopping the jobs is the helper's job.
// --------------------------------------------------------------------------------

/// Whether `pid` still has work to do.  A process that has died and not been reaped is
/// a zombie, and a container whose pid 1 does not wait() leaves them lying about, so a
/// zombie counts as dead here.  Signal 0 would have answered for it, and it has none.
fn process_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // "pid (comm) state ...", and a command name can hold spaces and parentheses,
        // so the fields that matter start after the last ')'.
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z')),
        Err(_) => false,
    }
}

/// Waits for a killed process to stop.  Dying is not instant, and the poll is what
/// keeps that from being a flake rather than a failure.
async fn process_stopped(pid: u32, how: &str) {
    let since = std::time::Instant::now();
    while process_alive(pid) {
        assert!(
            since.elapsed() < std::time::Duration::from_secs(10),
            "{how}: pid {pid} is still running"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Starts a command with a child of its own, and returns the pid of that child with
/// the spool file it shares with the shell.
///
/// The child is the point of the exercise.  It is in the job's group but not the pid
/// the helper spawned, so it only dies if the group is taken down.  It reports itself
/// before the shell waits, which tells a command that was killed apart from one that
/// never got as far as forking.
async fn job_with_a_child(helper: &mut Helper) -> (u32, String) {
    let started = helper
        .ok(
            "bash",
            serde_json::json!({"command": "sleep 120 & echo child=$!; wait", "refresh_sec": "0"}),
        )
        .await;
    assert!(started.contains("status=running"), "{started}");
    let spool = started
        .lines()
        .find_map(|line| line.strip_prefix("output_path="))
        .and_then(|rest| rest.split(" (").next())
        .expect("an output path")
        .to_string();

    let since = std::time::Instant::now();
    let child = loop {
        let text = std::fs::read_to_string(&spool).unwrap_or_default();
        if let Some(rest) = text.strip_prefix("child=") {
            break rest.trim().parse::<u32>().expect("a child pid");
        }
        assert!(
            since.elapsed() < std::time::Duration::from_secs(10),
            "the forked child never reported: {text}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert!(process_alive(child), "the child died on its own");
    (child, spool)
}

/// The agent's teardown closes stdin and escalates to SIGKILL for the helper's group,
/// so a job in a group of its own would be left running behind a dead helper, writing
/// into a spool file that the teardown has already removed.
#[tokio::test]
async fn a_session_that_ends_takes_its_running_jobs_with_it() {
    let mut helper = Helper::start(&[]).await;
    let (child, spool) = job_with_a_child(&mut helper).await;

    let status = helper.finish().await;
    assert!(
        status.success(),
        "the helper left through EOF badly: {status}"
    );
    process_stopped(child, "after the session ended").await;
    assert!(
        !std::path::Path::new(&spool).exists(),
        "spool file left behind: {spool}"
    );
}

/// The same teardown asked for with a signal instead of the closed stdin that usually
/// precedes it.  Dying on the spot would end the helper before it had stopped anything,
/// so the signal is a request to finish, and the agent's SIGKILL stays the backstop.
#[tokio::test]
async fn a_helper_that_is_signalled_stops_its_jobs_too() {
    let mut helper = Helper::start(&[]).await;
    let (child, spool) = job_with_a_child(&mut helper).await;

    let helper_pid = helper.child.id().expect("the helper has a pid");
    // stdin stays open, so nothing but the signal can end the session.
    unsafe { libc::kill(helper_pid as i32, libc::SIGTERM) };
    let status = helper.child.wait().await.expect("wait for the helper");
    assert!(
        status.success(),
        "the helper left through SIGTERM badly: {status}"
    );

    process_stopped(child, "after SIGTERM").await;
    assert!(
        !std::path::Path::new(&spool).exists(),
        "spool file left behind: {spool}"
    );
}
