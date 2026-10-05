//! ds4-sandbox-helper: the sandbox side of `ds4-agent --sandbox`.
//!
//! Reads framed requests from stdin and writes framed responses to stdout, running
//! the tools the agent routes: read, more, write, edit, list, search, bash,
//! bash_status, bash_stop.  Nothing else is written to stdout.  Diagnostics go to
//! stderr, which the agent drains, keeps a tail of, and mirrors next to --trace;
//! anything a tool learned that belongs in front of the model is in its result text,
//! because that is the only channel the model reads.
//!
//! The protocol this speaks is specified in ds4's docs/SANDBOX.md.

mod budget;
mod files;
mod protocol;
mod server;
mod tools;
mod wire;

use std::process::ExitCode;

use tools::Config;

const USAGE: &str = "\
ds4-sandbox-helper — run tools for ds4-agent --sandbox

Usage:
  ds4-sandbox-helper [options]

Options:
  --read-lines N     lines a read returns when max_lines is omitted (default 120;
                     DS4_READ_LINES does the same job).  The agent picks its own
                     default from the model's context size and that is not
                     information a sandbox can see, so it is passed here instead.
  --edit-upto        allow the [upto] anchor in an edit's old text, matching an
                     agent started with --edit-upto (DS4_EDIT_UPTO=1 likewise).
  -h, --help         show this text
  --version          show the version

Arguments are read from stdin as '<byte count>\\n<json>' and answers are written to
stdout the same way.  Start it as the --sandbox command, for example:

  ds4-agent --sandbox 'ds4-sandbox-helper --read-lines 240' -p 'prompt'

Exit status: 0 when the agent closed the session, 1 when it stopped making sense,
2 when these arguments could not be read.";

fn main() -> ExitCode {
    let config = match parse_args(std::env::args().skip(1)) {
        Ok(Some(config)) => config,
        Ok(None) => return ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("ds4-sandbox-helper: {reason}");
            eprintln!("{}", USAGE.trim_end());
            return ExitCode::from(2);
        }
    };

    // enable_all: the tools wait on pipes, on child processes, and on timers, and a
    // runtime that has not enabled an driver refuses to wait on any of them.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("ds4-sandbox-helper: could not start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(server::serve(
        tokio::io::stdin(),
        tokio::io::stdout(),
        config,
    )) {
        server::Outcome::Finished => ExitCode::SUCCESS,
        server::Outcome::Fault(reason) => {
            eprintln!("ds4-sandbox-helper: {reason}");
            ExitCode::FAILURE
        }
    }
}

/// `Ok(None)` means the process should exit successfully without serving: --help and
/// --version were asked for, and they are not part of a session.
fn parse_args<I: Iterator<Item = String>>(args: I) -> Result<Option<Config>, String> {
    let mut config = Config {
        read_lines: std::env::var("DS4_READ_LINES")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(120),
        edit_upto: matches!(
            std::env::var("DS4_EDIT_UPTO").ok().as_deref(),
            Some("1") | Some("true") | Some("yes")
        ),
    };

    let mut args = args.peekable();
    // A sandbox command may be started by a shell that appends nothing at all, so an
    // empty argument list is the normal case rather than a mistake.
    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name.to_string(), Some(value.to_string())),
            None => (arg.clone(), None),
        };
        match name.as_str() {
            "-h" | "--help" => {
                println!("{}", USAGE.trim_end());
                // Listed here rather than hard-coded in the text above, so the help
                // cannot claim to serve a tool the dispatch does not have.
                println!("\nTools served: {}", tools::SANDBOX_TOOLS.join(", "));
                return Ok(None);
            }
            "--version" => {
                println!("ds4-sandbox-helper {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--edit-upto" => config.edit_upto = true,
            "--read-lines" => {
                let value = match inline.or_else(|| args.next()) {
                    Some(value) => value,
                    None => return Err(format!("{name} needs a value")),
                };
                config.read_lines = value
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| format!("{name} needs a number of lines, not {value:?}"))?;
                if config.read_lines <= 0 {
                    return Err("read-lines must be at least 1".into());
                }
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(Some(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Config, String> {
        match parse_args(list.iter().map(|s| s.to_string())) {
            Ok(Some(config)) => Ok(config),
            Ok(None) => Err("handled as a one-off request".into()),
            Err(err) => Err(err),
        }
    }

    #[test]
    fn the_defaults_are_the_conservative_ones() {
        let config = args(&[]).unwrap();
        assert_eq!(config.read_lines, 120);
        assert!(!config.edit_upto);
    }

    #[test]
    fn both_spellings_of_an_option_with_a_value_work() {
        assert_eq!(args(&["--read-lines", "240"]).unwrap().read_lines, 240);
        assert_eq!(args(&["--read-lines=80"]).unwrap().read_lines, 80);
        assert!(args(&["--edit-upto"]).unwrap().edit_upto);
    }

    #[test]
    fn a_bad_value_is_refused_rather_than_ignored() {
        assert!(args(&["--read-lines", "wide"]).is_err());
        assert!(args(&["--read-lines", "0"]).is_err());
        assert!(args(&["--read-lines"]).is_err());
        assert!(args(&["--nope"]).is_err());
    }

    #[test]
    fn help_and_version_are_not_session_starts() {
        assert!(
            parse_args(["--help".to_string()].into_iter())
                .unwrap()
                .is_none()
        );
        assert!(
            parse_args(["--version".to_string()].into_iter())
                .unwrap()
                .is_none()
        );
    }
}
