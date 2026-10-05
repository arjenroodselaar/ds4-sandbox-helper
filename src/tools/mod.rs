//! Tool dispatch for the sandbox side.
//!
//! The set of tools here is the set the agent is willing to route: everything the
//! agent keeps for itself (`view_image`, and any name it does not know) never arrives,
//! and nothing here reaches outside the box it was started in except by the paths the
//! model names.

use crate::protocol::Request;

pub mod bash;
pub mod edit;
pub mod list;
pub mod read;
pub mod search;
pub mod write;

/// The tools the agent routes here.  Kept next to the dispatch so the two cannot
/// drift: a name the agent routes that this list does not mention is a bug in one of
/// them.
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

/// Settings the agent knows and the sandbox cannot work out for itself.
///
/// `read_lines` is the one that matters.  The C agent picks its default read size from
/// the model's context window, which is information the sandbox does not have and
/// should not guess: a sandbox that read 500 lines because it assumed a big model
/// would overflow a small one.  It is therefore a startup argument.
#[derive(Debug, Clone)]
pub struct Config {
    pub read_lines: i64,
    pub edit_upto: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            read_lines: 120,
            edit_upto: false,
        }
    }
}

/// State that lives for the whole session rather than one request: where `more`
/// resumes, and which shell commands are still running.
#[derive(Default)]
pub struct Session {
    pub more: Option<read::MoreState>,
    pub jobs: bash::Jobs,
}

impl Session {
    pub fn finish(&mut self) {
        bash::finish(&mut self.jobs);
        self.more = None;
    }
}

/// Runs one request.  `Err` carries the sentence that follows the agent's own
/// "Tool error: " prefix, which is why none of these messages start with it.
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
                request.arg_or("max_lines", config.read_lines, 1, i64::from(i32::MAX)),
                whole,
                raw,
                0,
                false,
                &mut session.more,
                true,
            )
        }
        "more" => read::more(
            &mut session.more,
            request.arg_or("count", config.read_lines, 1, i64::from(i32::MAX)),
        ),
        "write" => write::write(request),
        "edit" => edit::edit(request, config.edit_upto),
        "list" => list::list(request),
        "search" => search::search(request),
        "bash" => bash::start(request, &mut session.jobs).await,
        "bash_status" => bash::status_tool(request, &mut session.jobs).await,
        "bash_stop" => bash::stop(request, &mut session.jobs).await,
        // The agent answers an unknown name itself and never asks, so reaching this
        // line means a newer agent is routing something this helper does not know.
        // Saying so is better than pretending the tool did nothing.
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
        // A name in SANDBOX_TOOLS that the dispatch does not handle would be
        // indistinguishable from an unknown tool at runtime, so it is checked here.
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
        let path = std::env::temp_dir().join(format!(
            "ds4-helper-mod-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let text = path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path);
        call(
            "write",
            &format!(r#""path":"{text}","content":"one\ntwo\n""#),
        )
        .await
        .unwrap();
        let read = call("read", &format!(r#""path":"{text}""#)).await.unwrap();
        assert!(read.contains("1 one\n"), "{read}");
        call("bash", r#""command":"true""#).await.unwrap();
        std::fs::remove_file(&path).unwrap();
    }
}
