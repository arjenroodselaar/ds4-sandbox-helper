//! The two message shapes carried inside frames, and the strictness that goes
//! with them.
//!
//! A frame is read for exactly two things: the `id` it answers and the text to
//! report.  Everything else is ignored on purpose, which is what lets a sandbox
//! grow its protocol without breaking an agent that never learned the new field.
//! The exceptions are the fields that carry meaning we cannot do without: a frame
//! with no numeric `id` cannot be matched to a request, and a response with no
//! boolean `ok` cannot be told from a successful one, so both end the session
//! rather than being guessed at.

use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The most lines a request may ask a bare `read` or `more` to return.  It is the
/// agent's own largest default, so the cap costs nothing in practice and keeps a
/// hand-written frame from asking for the file and then some.
pub const MAX_INJECTED_READ_LINES: i64 = 500;

/// A request from the agent.  Every argument value is a string, numbers and
/// booleans included, because that is what a parsed tool call holds: there is
/// nothing to un-quote.  Keeping the type that way also means the sandbox decides
/// what `"timeout_sec":"30"` means, not the process that sent it.
#[derive(Debug, Clone)]
pub struct Request {
    pub id: i64,
    pub tool: String,
    pub args: BTreeMap<String, String>,
    /// The caps the sender sent with the request, in its own object.
    pub limits: Limits,
}

/// The `limits` object of a request: what the sender is allowed to tell us about
/// the model on the other side of the pipe, kept apart from `args` because it is
/// not model input and does not have to be a string.
///
/// A member that is absent says nothing, and a member this version does not know is
/// ignored rather than rejected, so a newer agent can name another cap without
/// ending the session of an older helper.
#[derive(Debug, Default, Clone, Copy)]
pub struct Limits {
    /// How much a `read` or `more` that was not told a size should return, in
    /// lines.  The agent knows this and we cannot: it follows the size of the model
    /// context this session is filling, and a helper that guessed would either
    /// overflow a small model or starve a big one.
    pub read_lines: Option<i64>,
}

impl Limits {
    fn parse(given: Option<&Value>) -> Self {
        let Some(Value::Object(members)) = given else {
            return Self::default();
        };
        Self {
            read_lines: members.get("read_lines").and_then(as_whole),
        }
    }
}

impl Request {
    /// The value of `key`, or `None` when the argument was omitted.  Absent and
    /// empty are different things to these tools: `""` was written by the model.
    pub fn arg(&self, key: &str) -> Option<&str> {
        self.args.get(key).map(String::as_str)
    }

    /// An argument as a number of lines, seconds or results, falling back to the
    /// tool's default when it is absent *or* unreadable.  The C agent is generous
    /// here because the values come from a language model: a model that writes
    /// `"max_lines": "plenty"` must not lose the whole call.
    pub fn arg_or(&self, key: &str, default: i64, min: i64, max: i64) -> i64 {
        match self.arg(key).and_then(|v| v.trim().parse::<i64>().ok()) {
            Some(v) => v.clamp(min, max),
            None => default,
        }
    }

    /// The size to answer a bare `read` or `more` with.
    ///
    /// The number the agent sends wins, because it is the only one of the two that
    /// knows what the model can hold; `fallback` is the helper's own setting, used
    /// when the sender says nothing, which in practice means a person driving this
    /// by hand.  What arrives is capped, because a helper that lets the other end ask
    /// for ten million lines has handed its output limit to whoever sent it, and
    /// 500 is the largest size the agent itself will ever ask for.
    pub fn read_lines_or(&self, fallback: i64) -> i64 {
        match self.limits.read_lines {
            Some(lines) => lines.clamp(1, MAX_INJECTED_READ_LINES),
            None => fallback.max(1),
        }
    }

    pub fn bool_or(&self, key: &str, default: bool) -> bool {
        match self.arg(key).map(|v| v.trim().to_ascii_lowercase()) {
            None => default,
            Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => true,
            Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => false,
            Some(_) => default,
        }
    }
}

/// A framing violation that ends the session: the peer is not answering requests.
#[derive(Debug)]
pub enum ProtocolError {
    NotAnObject,
    MissingId,
    MissingTool,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::NotAnObject => write!(f, "frame payload is not a JSON object"),
            ProtocolError::MissingId => write!(f, "frame has no numeric id"),
            ProtocolError::MissingTool => write!(f, "request has no tool name"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Parses a request frame.  Values that are not strings are kept as their compact
/// JSON text: an agent that ever starts sending `"timeout_sec":30` still gets a
/// sandbox that understands it.
pub fn parse_request(payload: &[u8]) -> Result<Request, ProtocolError> {
    let value: Value = serde_json::from_slice(payload).map_err(|_| ProtocolError::NotAnObject)?;
    let Value::Object(fields) = value else {
        return Err(ProtocolError::NotAnObject);
    };
    let id = match fields.get("id") {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .ok_or(ProtocolError::MissingId)?,
        _ => return Err(ProtocolError::MissingId),
    };
    let Some(Value::String(tool)) = fields.get("tool") else {
        return Err(ProtocolError::MissingTool);
    };
    let mut args = BTreeMap::new();
    if let Some(Value::Object(given)) = fields.get("args") {
        for (key, value) in given {
            let text = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            // A repeated parameter keeps its first value, which is the one the
            // agent's own argument lookup would have handed to a local tool.
            args.entry(key.clone()).or_insert(text);
        }
    }
    let limits = Limits::parse(fields.get("limits"));
    Ok(Request {
        id,
        tool: tool.clone(),
        args,
        limits,
    })
}

/// A JSON value read as a whole number.  The agent sends limits as numbers, but
/// every other value on this wire is a string, and a hand-written frame that mixes
/// the two should still work.
fn as_whole(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// A successful answer.  `result` is already formatted for the model: the agent
/// wraps it in the `Tool result N (name):` header verbatim.
pub fn response_ok(id: i64, result: &str) -> Vec<u8> {
    json!({ "id": id, "ok": true, "result": result })
        .to_string()
        .into_bytes()
}

/// A failed tool.  The message is the sentence after the agent's own
/// "Tool error: " prefix, so it must read well standing there.
pub fn response_error(id: i64, error: &str) -> Vec<u8> {
    json!({ "id": id, "ok": false, "error": error })
        .to_string()
        .into_bytes()
}

/// A notice: diagnostics for whoever reads `<trace>.sandbox.log`.  `id` 0 is never
/// a request, and a notice never completes one, so this cannot answer early.
pub fn notice(text: &str) -> Vec<u8> {
    json!({ "id": 0, "type": "log", "text": text })
        .to_string()
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_read_exactly_as_documented() {
        let req = parse_request(
            br#"{"id":7,"tool":"read","args":{"path":"ds4_agent.c","start_line":"120"}}"#,
        )
        .unwrap();
        assert_eq!(req.id, 7);
        assert_eq!(req.tool, "read");
        assert_eq!(req.arg("path"), Some("ds4_agent.c"));
        assert_eq!(req.arg("start_line"), Some("120"));
        assert_eq!(req.arg("whole"), None);
    }

    #[test]
    fn unknown_members_are_ignored_and_absent_args_stay_absent() {
        let req =
            parse_request(br#"{"id":1,"tool":"list","args":{},"future":{"a":[1,2]},"note":"hi"}"#)
                .unwrap();
        assert_eq!(req.tool, "list");
        assert!(req.args.is_empty());
        assert_eq!(req.arg("note"), None);
    }

    #[test]
    fn a_number_is_not_an_id_and_an_object_is_not_a_frame() {
        assert!(matches!(
            parse_request(br#"{"id":"7","tool":"read"}"#),
            Err(ProtocolError::MissingId)
        ));
        assert!(matches!(
            parse_request(br#"{"tool":"read"}"#),
            Err(ProtocolError::MissingId)
        ));
        assert!(matches!(
            parse_request(br#"[{"id":1}]"#),
            Err(ProtocolError::NotAnObject)
        ));
        assert!(matches!(
            parse_request(br#"{}"#),
            Err(ProtocolError::MissingId)
        ));
        assert!(matches!(
            parse_request(br#"not json"#),
            Err(ProtocolError::NotAnObject)
        ));
    }

    #[test]
    fn numeric_arguments_are_accepted_as_text() {
        let req = parse_request(br#"{"id":1,"tool":"bash","args":{"timeout_sec":30}}"#).unwrap();
        assert_eq!(req.arg("timeout_sec"), Some("30"));
    }

    #[test]
    fn a_duplicate_argument_still_yields_one_value() {
        // JSON has nothing to say about a repeated key.  The agent's parser keeps
        // the first and serde_json keeps the last; what matters is that a tool sees
        // one value rather than having to guess which the model meant.
        let req =
            parse_request(br#"{"id":1,"tool":"x","args":{"k":"first","k":"second"}}"#).unwrap();
        assert!(matches!(req.arg("k"), Some("first") | Some("second")));
        assert_eq!(req.args.len(), 1);
    }

    #[test]
    fn argument_defaults_respect_their_bounds() {
        let req = parse_request(
            br#"{"id":1,"tool":"read","args":{"max_lines":"99999","context":"-3","whole":"TRUE"}}"#,
        )
        .unwrap();
        assert_eq!(req.arg_or("max_lines", 50, 1, 500), 500);
        assert_eq!(req.arg_or("context", 0, 0, 5), 0, "below the minimum");
        assert_eq!(req.arg_or("missing", 24, 1, 500), 24);
        assert_eq!(req.arg_or("whole", 24, 1, 500), 24, "not a number at all");
        assert!(req.bool_or("whole", false));
        assert!(req.bool_or("unset", true));
        assert!(!req.bool_or("unset", false));
    }

    #[test]
    fn responses_carry_the_fields_the_agent_demands() {
        let ok: Value = serde_json::from_slice(&response_ok(9, "done\n")).unwrap();
        assert_eq!(ok["id"], 9);
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["result"], "done\n");
        let err: Value = serde_json::from_slice(&response_error(9, "no such file")).unwrap();
        assert_eq!(err["ok"], false);
        assert_eq!(err["error"], "no such file");
        let log: Value = serde_json::from_slice(&notice("hello")).unwrap();
        assert_eq!(log["id"], 0);
        assert_eq!(log["type"], "log");
    }

    #[test]
    fn text_survives_the_wire() {
        let odd = "quotes \" backslash \\ newline \n tab \t unicode 中 😀 \u{1}";
        let ok: Value = serde_json::from_slice(&response_ok(1, odd)).unwrap();
        assert_eq!(ok["result"].as_str().unwrap(), odd);
    }

    #[test]
    fn the_injected_read_size_wins_and_is_capped() {
        let parse = |frame: &str| parse_request(frame.as_bytes()).unwrap();
        assert_eq!(
            parse(r#"{"id":1,"tool":"read","limits":{"read_lines":240},"args":{}}"#)
                .read_lines_or(120),
            240
        );
        // A sender that says nothing falls back to the helper's own setting.
        assert_eq!(
            parse(r#"{"id":1,"tool":"read","args":{}}"#).read_lines_or(120),
            120
        );
        assert_eq!(
            parse(r#"{"id":1,"tool":"read","limits":{"read_lines":"0"},"args":{}}"#)
                .read_lines_or(120),
            1
        );
        assert_eq!(
            parse(r#"{"id":1,"tool":"read","limits":{"read_lines":999999},"args":{}}"#)
                .read_lines_or(120),
            crate::protocol::MAX_INJECTED_READ_LINES
        );
        // Text is accepted because text is what the rest of the frame is made of,
        // and nonsense is the same as absent rather than an error the model caused.
        assert_eq!(
            parse(r#"{"id":1,"tool":"read","limits":{"read_lines":"many"},"args":{}}"#)
                .read_lines_or(120),
            120
        );
        assert_eq!(
            parse(r#"{"id":1,"tool":"read","limits":{"read_lines":"60"},"args":{}}"#)
                .read_lines_or(120),
            60
        );
        // A cap this version does not know, and a limits object that is not one,
        // are both ignored: a request is not rejected over a member it cannot read.
        assert_eq!(
            parse(r#"{"id":1,"tool":"read","limits":{"max_bytes":10},"args":{}}"#)
                .read_lines_or(120),
            120
        );
        assert_eq!(
            parse(r#"{"id":1,"tool":"read","limits":"500","args":{}}"#).read_lines_or(120),
            120
        );
        // A limit is not an argument, and does not arrive in the tool's own view of
        // the call.
        assert_eq!(
            parse(r#"{"id":1,"tool":"read","limits":{"read_lines":240},"args":{}}"#).arg_or(
                "max_lines",
                120,
                1,
                500
            ),
            120
        );
    }
}
