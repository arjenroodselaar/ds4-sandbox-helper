// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `bash`, `bash_status` and `bash_stop` run shell commands that outlive one request.
//!
//! A command that takes four minutes must not make the model wait four minutes to
//! learn that it started.  So the command runs in its own process group with its
//! output spooled to a file, and every answer has the same shape: the first shows the
//! head of the output, later ones the tail, and `read` can be pointed at the file.
//!
//! Each job gets a task that awaits its child and enforces the deadline, and a waiter
//! subscribes to a watch channel instead of polling, so a finished job wakes whoever
//! is waiting for it.  Output goes straight to the spool file rather than through a
//! pipe, which is one fewer buffer that can fill and stall the command.

use std::fmt::Write;
use std::io::SeekFrom;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use tempfile::NamedTempFile;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::process::Command;
use tokio::sync::watch;

use crate::budget::Budget;
use crate::budget::MAX_TOOL_BYTES;
use crate::files;
use crate::protocol::Request;

const HEAD_BYTES: u64 = 8 * 1024;
const HEAD_LINES: usize = 100;
const TAIL_BYTES: u64 = 32 * 1024;
const PROGRESS_TAIL_LINES: usize = 4;
const FINAL_TAIL_LINES: usize = 20;
const DEFAULT_TIMEOUT_SEC: f64 = 3600.0;
const MAX_TIMEOUT_SEC: f64 = 86_400.0;
/// How long a stopped job is given to notice the request before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(1);
/// How long to wait for the reaper when `bash_stop` was not told how long to wait.
/// It is a ceiling, not a delay.  The answer goes out the moment the job is reaped.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// What the watcher task knows about a job, cloned into every observation.
#[derive(Debug, Clone)]
pub struct Status {
    pub running: bool,
    pub exit_status: i32,
    pub timed_out: bool,
    /// Output the spool file could not hold.  Reported, never quietly dropped.
    pub output_error: Option<String>,
    /// When the child exited, so elapsed time stops there.
    pub ended_at: Option<Instant>,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            running: true,
            exit_status: 0,
            timed_out: false,
            output_error: None,
            ended_at: None,
        }
    }
}

pub struct Job {
    pub id: i32,
    pub pid: u32,
    pub path: PathBuf,
    pub started: Instant,
    pub timeout_sec: f64,
    pub status: watch::Sender<Status>,
    /// The first answer shows the head of the output, later ones the tail.
    pub observed_once: bool,
}

impl Job {
    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    fn elapsed(&self, status: &Status) -> f64 {
        match status.ended_at {
            Some(end) => end.duration_since(self.started).as_secs_f64(),
            None if status.running => self.started.elapsed().as_secs_f64(),
            None => 0.0,
        }
    }
}

/// The jobs the agent may ask about, owned by the session.
#[derive(Default)]
pub struct Jobs {
    pub next_id: i32,
    pub list: Vec<Job>,
}

impl Jobs {
    /// By id, falling back to pid only when no id was given.  A pid is not a stable
    /// handle once the job has been reaped.
    fn find_index(&self, id: i32, pid: i64) -> Option<usize> {
        self.list.iter().position(|job| {
            (id > 0 && job.id == id) || (id <= 0 && pid > 0 && job.pid as i64 == pid)
        })
    }

    async fn remove(&mut self, id: i32) {
        // A finished job's spool file goes with it.  The agent has already seen the
        // output, and /tmp is not this process's to keep.
        if let Some(index) = self.list.iter().position(|job| job.id == id) {
            let job = self.list.remove(index);
            let _ = tokio::fs::remove_file(&job.path).await;
        }
    }
}

/// Starts a command and reports the first snapshot, waiting at most `refresh_sec` for
/// a command that finishes quickly.  The shell is the one settled on at startup.  A
/// model that could name an interpreter would eventually name one that is not there.
pub async fn start(request: &Request, jobs: &mut Jobs, shell: &Path) -> Result<String, String> {
    let Some(command) = request.arg("command").filter(|c| !c.is_empty()) else {
        return Err("bash requires command".into());
    };
    let timeout = request
        .arg("timeout_sec")
        .map(str::trim)
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_TIMEOUT_SEC)
        .min(MAX_TIMEOUT_SEC);
    let refresh = request.arg_or("refresh_sec", 60, 1, 3600) as u64;

    if jobs.next_id <= 0 {
        jobs.next_id = 1;
    }
    let id = jobs.next_id;
    jobs.next_id += 1;

    let (file, path) = spool_file()
        .await
        .map_err(|err| format!("bash failed to start: could not create output file: {err}"))?;
    let stderr_file = file
        .try_clone()
        .await
        .map_err(|err| format!("bash failed to start: {err}"))?;

    let mut child = Command::new(shell)
        // The one flag every shell worth choosing understands.
        .arg("-c")
        .arg(command)
        // The helper's stdin is the request stream.  A command must not eat a frame.
        .stdin(Stdio::null())
        // Plain descriptors, because the helper never reads them back.
        .stdout(Stdio::from(file.into_std().await))
        .stderr(Stdio::from(stderr_file.into_std().await))
        // A forked command leaves children behind, and a survivor keeps writing into
        // the spool after the job was reported stopped.
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| {
            // Name the shell, so ENOENT does not read as a missing file in the command.
            format!(
                "bash failed to start: {} could not be run: {}",
                shell.display(),
                files::err_message(&err)
            )
        })?;

    let pid = child.id().unwrap_or(0);
    let (sender, _receiver) = watch::channel(Status::default());
    let finished = sender.clone();
    tokio::spawn(async move {
        let status =
            match tokio::time::timeout(Duration::from_secs_f64(timeout), child.wait()).await {
                Ok(Ok(exit)) => Status {
                    running: false,
                    exit_status: exit_status_of(&exit),
                    timed_out: false,
                    output_error: None,
                    ended_at: Some(Instant::now()),
                },
                Ok(Err(err)) => Status {
                    running: false,
                    exit_status: -1,
                    timed_out: false,
                    output_error: Some(err.to_string()),
                    ended_at: Some(Instant::now()),
                },
                Err(_elapsed) => {
                    kill_group(pid, libc::SIGTERM);
                    // A moment to exit orderly before the uncatchable signal.
                    tokio::time::sleep(STOP_GRACE).await;
                    kill_group(pid, libc::SIGKILL);
                    let exit = child.wait().await.ok();
                    Status {
                        running: false,
                        // Reap the job just killed, so the report says which signal ended it.
                        exit_status: exit.as_ref().map(exit_status_of).unwrap_or(-1),
                        timed_out: true,
                        output_error: None,
                        ended_at: Some(Instant::now()),
                    }
                }
            };
        let _ = finished.send(status);
    });

    jobs.list.push(Job {
        id,
        pid,
        path,
        started: Instant::now(),
        timeout_sec: timeout,
        status: sender,
        observed_once: false,
    });
    let index = jobs.list.len() - 1;

    wait_for(&mut jobs.list[index], Duration::from_secs(refresh)).await;
    let text = observation(&mut jobs.list[index]).await;
    if !jobs.list[index].status().running {
        jobs.remove(id).await;
    }
    Ok(text)
}

/// Reports on a job, waiting up to `refresh_sec` for it to change state first.
pub async fn status_tool(request: &Request, jobs: &mut Jobs) -> Result<String, String> {
    let (id, pid) = requested_job(request);
    let Some(index) = jobs.find_index(id, pid) else {
        return Err(format!("bash job not found: job={id} pid={pid}"));
    };
    let refresh = request.arg_or("refresh_sec", 0, 0, 3600) as u64;
    if refresh > 0 {
        wait_for(&mut jobs.list[index], Duration::from_secs(refresh)).await;
    }
    let text = observation(&mut jobs.list[index]).await;
    if !jobs.list[index].status().running {
        jobs.remove(id).await;
    }
    Ok(text)
}

pub async fn stop(request: &Request, jobs: &mut Jobs) -> Result<String, String> {
    let (id, pid) = requested_job(request);
    let Some(index) = jobs.find_index(id, pid) else {
        return Err(format!("bash job not found: job={id} pid={pid}"));
    };
    // A stop's own patience is a ceiling, not a delay.  With no `refresh_sec` it still
    // allows a second, and a job that dies at once is reported at once.
    let refresh = request.arg_or("refresh_sec", 0, 0, 3600);
    let patience = if refresh > 0 {
        Duration::from_secs(refresh as u64)
    } else {
        STOP_WAIT
    };

    if jobs.list[index].status().running {
        let pid = jobs.list[index].pid;
        kill_group(pid, libc::SIGTERM);
        wait_for(&mut jobs.list[index], STOP_GRACE).await;
        if jobs.list[index].status().running {
            kill_group(pid, libc::SIGKILL);
        }
    }
    // The reaper still has to notice, or the answer would say "running" for a process
    // that is already reaped.
    wait_for(&mut jobs.list[index], patience).await;
    let text = observation(&mut jobs.list[index]).await;
    if !jobs.list[index].status().running {
        jobs.remove(id).await;
    }
    Ok(text)
}

/// Forgets every job and removes its spool file, for the session teardown path.
pub async fn finish(jobs: &mut Jobs) {
    for job in jobs.list.drain(..) {
        let _ = tokio::fs::remove_file(&job.path).await;
    }
}

fn requested_job(request: &Request) -> (i32, i64) {
    (
        request.arg_or("job", 0, 0, i64::from(i32::MAX)) as i32,
        request.arg_or("pid", 0, 0, i64::from(i32::MAX)),
    )
}

/// The exit code, 128+signal when a signal ended the process, or -1 when neither is
/// known.  A stopped job reads as 143, which is what a shell user expects.
fn exit_status_of(exit: &std::process::ExitStatus) -> i32 {
    match exit.code() {
        Some(code) => code,
        None => exit.signal().map_or(-1, |sig| 128 + sig),
    }
}

/// Waits until the job is no longer running, or until `limit` has passed.
async fn wait_for(job: &mut Job, limit: Duration) {
    if !job.status().running {
        return;
    }
    let mut receiver = job.status.subscribe();
    let _ = tokio::time::timeout(limit, receiver.wait_for(|s| !s.running)).await;
}

/// The snapshot the model sees, in the same fields and order the agent's own output uses.
async fn observation(job: &mut Job) -> String {
    let status = job.status();
    let first = !job.observed_once;
    job.observed_once = true;

    let mut out = Budget::new(MAX_TOOL_BYTES.saturating_sub(4096));
    let elapsed = job.elapsed(&status);
    if status.running {
        let _ = writeln!(
            out,
            "bash job={} pid={} status=running elapsed_sec={:.1} timeout_sec={:.0}",
            job.id, job.pid, elapsed, job.timeout_sec
        );
    } else {
        let _ = writeln!(
            out,
            "bash job={} pid={} status=done elapsed_sec={:.1} timed_out={}",
            job.id,
            job.pid,
            elapsed,
            i32::from(status.timed_out)
        );
        let _ = writeln!(out, "exit_status={}", status.exit_status);
    }
    if let Some(err) = &status.output_error {
        let _ = writeln!(
            out,
            "Tool error: command output could not be captured completely: {err}"
        );
    }

    let path_text = job.path.to_string_lossy().into_owned();
    let bytes = tokio::fs::metadata(&job.path)
        .await
        .map(|meta| meta.len() as usize)
        .unwrap_or(0);
    let lines = count_lines(&job.path).await;

    if bytes == 0 {
        let _ = write!(out, "<output>\n</output>\n");
    } else if first {
        let (head, shown, byte_limited) = read_head(&job.path).await;
        let truncated = byte_limited || lines > shown;
        if !status.running && !truncated {
            // Small and finished, so this is the answer rather than an excerpt of a file.
            let _ = write!(out, "<output>\n{head}");
            if !head.is_empty() && !head.ends_with('\n') {
                let _ = writeln!(out);
            }
            let _ = writeln!(out, "</output>");
        } else {
            let _ = write!(
                out,
                "output_path={path_text} ({bytes} bytes, {lines} lines)\n<head -{HEAD_LINES} {path_text}>\n{head}"
            );
            if !head.is_empty() && !head.ends_with('\n') {
                let _ = writeln!(out);
            }
            let _ = writeln!(out, "</head>");
        }
    } else {
        let want = if status.running {
            PROGRESS_TAIL_LINES
        } else {
            FINAL_TAIL_LINES
        };
        let tail = read_tail(&job.path, want).await;
        let _ = write!(
            out,
            "output_path={path_text} ({bytes} bytes, {lines} lines)\n<tail -{want} {path_text}>\n{tail}"
        );
        if !tail.is_empty() && !tail.ends_with('\n') {
            let _ = writeln!(out);
        }
        let _ = writeln!(out, "</tail>");
    }

    if status.running {
        let _ = write!(
            out,
            "\nUse bash_status job={} to get info before refresh time; use bash_stop job={} to stop execution\n",
            job.id, job.id
        );
    }
    out.into_string()
}

/// The file a job's output is spooled into, named `ds4_agent_output_XXXXXX` like the
/// agent's own, so a sandbox log and an agent log say the same thing.
///
/// Owner read/write only, and not deleted on exit.  The model reads it back by path
/// long after the command is gone.
async fn spool_file() -> Result<(File, PathBuf), String> {
    tokio::task::spawn_blocking(|| {
        let temp = NamedTempFile::with_prefix_in("ds4_agent_output_", std::env::temp_dir())
            .map_err(|err| files::err_message(&err))?;
        let (file, path) = temp.keep().map_err(|err| files::err_message(&err.error))?;
        Ok((File::from(file), path))
    })
    .await
    .map_err(|err| format!("the output file was not created: {err}"))?
}

fn kill_group(pid: u32, signal: i32) {
    if pid == 0 {
        return;
    }
    // Negative on purpose, so the whole group goes and not only the shell that exec'd.
    unsafe { libc::killpg(pid as i32, signal) };
}

/// Newlines plus one for a trailing partial line.  Streamed, because a command can
/// write more than memory.
async fn count_lines(path: &Path) -> usize {
    let Ok(mut file) = File::open(path).await else {
        return 0;
    };
    let mut lines = 0usize;
    let mut last = None;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                for byte in &buf[..n] {
                    if *byte == b'\n' {
                        lines += 1;
                    }
                }
                last = Some(buf[n - 1]);
            }
            Err(_) => break,
        }
    }
    if last.is_some_and(|byte| byte != b'\n') {
        lines += 1;
    }
    lines
}

/// The first lines of the output, capped at [`HEAD_BYTES`].  Reports what stopped
/// it, because head-versus-whole-output is decided by that.
async fn read_head(path: &Path) -> (String, usize, bool) {
    let Ok(mut file) = File::open(path).await else {
        return ("<failed to reopen output file>\n".to_string(), 0, false);
    };
    let mut buf = vec![0u8; HEAD_BYTES as usize];
    let mut taken = 0;
    while taken < buf.len() {
        match file.read(&mut buf[taken..]).await {
            Ok(0) => break,
            Ok(n) => taken += n,
            Err(_) => break,
        }
    }
    let total = tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let window = &buf[..taken];
    let mut lines = 0;
    let mut cut = window.len();
    for (index, byte) in window.iter().enumerate() {
        if *byte == b'\n' {
            lines += 1;
            if lines >= HEAD_LINES {
                cut = index + 1;
                break;
            }
        }
    }
    let shown = window[..cut].iter().filter(|b| **b == b'\n').count()
        + usize::from(!window[..cut].is_empty() && window[cut - 1] != b'\n');
    let byte_limited = taken as u64 >= HEAD_BYTES && total > HEAD_BYTES;
    (
        String::from_utf8_lossy(&window[..cut]).into_owned(),
        shown,
        byte_limited,
    )
}

/// The last `want` lines, read from the end so a long log costs a fixed amount.
async fn read_tail(path: &Path, want: usize) -> String {
    let Ok(mut file) = File::open(path).await else {
        return "<failed to reopen output file>\n".to_string();
    };
    let Ok(meta) = file.metadata().await else {
        return String::new();
    };
    let len = meta.len();
    let take = std::cmp::min(len, TAIL_BYTES);
    if file.seek(SeekFrom::End(-(take as i64))).await.is_err() {
        return "<failed to seek output file>\n".to_string();
    }
    let mut window = vec![0u8; take as usize];
    let mut read = 0;
    while read < window.len() {
        match file.read(&mut window[read..]).await {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => break,
        }
    }
    let window = &window[..read];
    // A fragment at the start of the window is not a line, so it is not shown.
    let starts_partial = take < len && !window.starts_with(b"\n");
    let mut start = 0;
    let mut seen = 0;
    for index in (0..window.len()).rev() {
        if window[index] != b'\n' {
            continue;
        }
        seen += 1;
        if starts_partial && seen == 1 {
            continue;
        }
        if seen > want {
            start = index + 1;
            break;
        }
        start = index + 1;
    }
    String::from_utf8_lossy(&window[start..]).into_owned()
}
