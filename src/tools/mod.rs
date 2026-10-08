//! Tool dispatch for the sandbox side.
//!
//! The agent routes nothing here that it does not list, and nothing here reaches
//! outside the box except by the paths the model names.

use std::path::PathBuf;

use crate::protocol::Request;

pub mod bash;
pub mod edit;
pub mod list;
pub mod read;
pub mod search;
pub mod write;

/// The tools the agent routes here, kept next to the dispatch so the two cannot drift.
pub const SANDBOX_TOOLS: [&str; 9] = [
    "read",
    "more",
    "write",
    "edit",
    "list",
    "search",
    "bash",
    "bash_status",
    "bash_stop",
];

/// Default shell: models write their commands for bash, so `[[ ]]` and arrays work.
pub const BASH_SHELL: &str = "/bin/bash";

pub const FALLBACK_SHELL: &str = "/bin/sh";

/// Settings a request does not carry.  `read_lines` is what a bare `read` returns when
/// the sender sends no `limits`, which in practice means a person at a terminal.
#[derive(Debug, Clone)]
pub struct Config {
    pub read_lines: i64,
    pub edit_upto: bool,
    pub shell: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            read_lines: 120,
            edit_upto: false,
            shell: PathBuf::from(FALLBACK_SHELL),
        }
    }
}

/// State that lives for the session, not one request: where `more` resumes, and which
/// shell commands are running.
#[derive(Default)]
pub struct Session {
    pub more: Option<read::MoreState>,
    pub jobs: bash::Jobs,
}

impl Session {
    pub async fn finish(&mut self) {
        bash::finish(&mut self.jobs).await;
        self.more = None;
    }
}

/// Runs one request.  `Err` is the sentence after the agent's "Tool error: " prefix,
/// which is why none of these messages start with it.
pub async fn run(
    request: &Request,
    session: &mut Session,
    config: &Config,
) -> Result<String, String> {
    match request.tool.as_str() {
        "read" => {
            let whole = request.bool_or("whole", false);
            let raw = request.bool_or("raw", false);
            read::read_range(
                request.arg("path").unwrap_or(""),
                request.arg_or("start_line", 1, 1, i64::from(i32::MAX)),
                request.arg_or(
                    "max_lines",
                    request.read_lines_or(config.read_lines),
                    1,
                    i64::from(i32::MAX),
                ),
                whole,
                raw,
                0,
                false,
                &mut session.more,
                true,
            )
            .await
        }
        "more" => {
            read::more(
                &mut session.more,
                request.arg_or(
                    "count",
                    request.read_lines_or(config.read_lines),
                    1,
                    i64::from(i32::MAX),
                ),
            )
            .await
        }
        "write" => write::write(request).await,
        "edit" => edit::edit(request, config.edit_upto).await,
        "list" => list::list(request).await,
        "search" => search::search(request).await,
        "bash" => bash::start(request, &mut session.jobs, &config.shell).await,
        "bash_status" => bash::status_tool(request, &mut session.jobs).await,
        "bash_stop" => bash::stop(request, &mut session.jobs).await,
        // The agent answers unknown names itself, so this is a newer agent routing
        // a tool this helper does not have.
        other => Err(format!("unknown tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::parse_request;

    async fn call(tool: &str, args: &str) -> Result<String, String> {
        let request =
            parse_request(format!(r#"{{"id":1,"tool":"{tool}","args":{{{args}}}}}"#).as_bytes())
                .unwrap();
        run(&request, &mut Session::default(), &Config::default()).await
    }

    #[tokio::test]
    async fn an_unknown_tool_says_which_one() {
        let err = call("teleport", "").await.unwrap_err();
        assert_eq!(err, "unknown tool: teleport");
    }

    #[tokio::test]
    async fn every_routed_name_reaches_its_tool() {
        // A name routed but not dispatched would look like an unknown tool.
        for name in SANDBOX_TOOLS {
            let result = call(name, r#""path":"/definitely/not/here""#).await;
            let err = result.unwrap_err();
            assert!(
                err != format!("unknown tool: {name}"),
                "{name} is routed but not dispatched"
            );
        }
    }

    #[tokio::test]
    async fn a_round_trip_through_the_dispatch_works() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("file").to_str().unwrap().to_string();
        call(
            "write",
            &format!(r#""path":"{text}","content":"one\ntwo\n""#),
        )
        .await
        .unwrap();
        let read = call("read", &format!(r#""path":"{text}""#)).await.unwrap();
        assert!(read.contains("1 one\n"), "{read}");
        call("bash", r#""command":"true""#).await.unwrap();
    }
}
