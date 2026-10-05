//! End-to-end tests: the real binary, real pipes, real files, real processes.
//!
//! These drive the helper exactly as `ds4-agent --sandbox` does — framed JSON on
//! stdin, framed JSON on stdout — so they check the thing unit tests cannot: that the
//! byte counts agree with the payloads, that a response always arrives for a request,
//! and that a tool's answer survives the trip through the protocol without losing its
//! shape.  A helper that passes its own module tests but whose framing is off by one
//! byte would hang the agent, and only a test like this one notices.
//!
//! The codec here is written again on purpose: an integration test that imported the
//! binary's own encoder could not catch an encoder that disagrees with the spec.

use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Diagnostics stay on the inherited stderr: the agent's contract is that
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
        // The first frame is the helper's own startup notice, and there is nothing
        // else on the stream yet: waiting past it here would wait for a reply that no
        // request has asked for.
        let first = helper.one().await;
        assert_eq!(first["type"], "log", "expected a notice first, got {first}");
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

    /// Reads the next frame that answers something.  A helper may speak on its own
    /// initiative at any point, and the agent treats those as notices rather than as
    /// replies, so the test has to do the same and keep looking.
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
    /// what the agent sends: numbers and booleans included.
    async fn call(&mut self, tool: &str, args: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id;
        let request = serde_json::json!({ "id": id, "tool": tool, "args": args });
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

    /// Closes stdin and waits: the helper should treat that as the end of the run.
    async fn finish(self) -> std::process::ExitStatus {
        let Helper {
            mut child,
            stdin,
            stdout,
            ..
        } = self;
        // stdout has to go before stdin: some operating systems wake a blocked reader
        // before they deliver EOF on the other direction, and the helper would keep
        // reading frames otherwise.
        drop(stdout);
        drop(stdin);
        child.wait().await.expect("wait for the helper")
    }
}

fn scratch(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "ds4-helper-e2e-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("scratch directory");
    path
}

#[tokio::test]
async fn a_write_and_a_read_round_trip_through_the_wire() {
    let dir = scratch("write");
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
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn more_resumes_a_long_file_where_read_stopped() {
    let dir = scratch("more");
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
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn an_edit_reports_the_span_it_touched() {
    let dir = scratch("edit");
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
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn list_and_search_answer_over_the_same_channel() {
    let dir = scratch("tree");
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
    std::fs::remove_dir_all(dir).ok();
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

    // refresh_sec=0 asks not to wait for it, which is how a long build is started.
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

    // The job and its spool file are gone once the model has been shown the end:
    // the sandbox is not a place to accumulate state between runs.
    let gone = helper
        .fail("bash_status", serde_json::json!({"job": job.as_str()}))
        .await;
    assert!(gone.starts_with("bash job not found"), "{gone}");
    assert!(!std::path::Path::new(&spool).exists(), "spool file left");

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
    helper.finish().await;
}

#[tokio::test]
async fn a_shell_cannot_read_the_request_stream() {
    // The helper's stdin is the agent's frame stream.  A command that inherited it
    // would consume the next request and the session would desynchronise, which is
    // the single most dangerous thing a sandbox tool can do.
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

#[tokio::test]
async fn a_request_for_an_unknown_tool_is_answered_not_ignored() {
    let mut helper = Helper::start(&[]).await;
    let error = helper
        .fail("view_image", serde_json::json!({"path": "x"}))
        .await;
    assert_eq!(error, "unknown tool: view_image");
    // The session survives it: an error is an answer, not a fault.
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
    // Not a byte count at all: there is no way to know where the next frame starts.
    stdin.write_all(b"this is not a frame\n").await.unwrap();
    stdin.flush().await.unwrap();
    let status = child.wait().await.expect("wait");
    assert!(!status.success(), "a garbage frame must not exit clean");
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
    let dir = scratch("limit");
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
    std::fs::remove_dir_all(dir).ok();
}
