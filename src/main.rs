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

use clap::{ArgAction, Parser};
use tools::Config;

// The command line, which is also what --help prints.  The doc comment on the struct
// would become help text, and these two fields are not the sort of thing to tell a
// user about, so the words below are chosen for the reader instead.
#[derive(Debug, Parser)]
#[command(
    version,
    about = "run tools for ds4-agent --sandbox",
    long_about = "Run tools for a ds4-agent that has no filesystem or shell of its own.\n\
        \n\
        Everything the model asks for arrives with the request, as does everything the \
        agent knows about the model; the options below are what is left, which is what \
        only whoever started this helper can decide.",
    after_help = after_help(),
)]
struct Args {
    /// Lines a read returns when max_lines is omitted
    ///
    /// The agent works its own number out from the size of the model's context and
    /// sends it with every request, which is the only way either side can know it,
    /// so this is the answer for a sender that says nothing: a person at a terminal.
    #[arg(
        long,
        value_name = "N",
        env = "DS4_READ_LINES",
        default_value_t = Config::default().read_lines,
        value_parser = clap::value_parser!(i64).range(1..),
    )]
    read_lines: i64,

    /// Allow the [upto] anchor in an edit's old text
    ///
    /// It matches everything between two anchors, which can delete a great deal at
    /// once, so it is opt-in.  Matches an agent started with --edit-upto.
    #[arg(
        long,
        env = "DS4_EDIT_UPTO",
        action = ArgAction::Set,
        num_args(0..=1),
        value_name = "BOOL",
        // Booleans only: the parser also accepts yes/no/on/off/1/0, so listing the
        // two it prints as "possible values" would understate what it takes.
        hide_possible_values = true,
        default_missing_value = "true",
        default_value_t = false,
        value_parser = clap::builder::BoolishValueParser::new(),
    )]
    edit_upto: bool,
}

impl Args {
    fn config(&self) -> Config {
        Config {
            read_lines: self.read_lines,
            edit_upto: self.edit_upto,
        }
    }
}

/// The part of the help text that cannot be written where it is used, because it
/// names the tools the dispatch actually serves: the help cannot then claim to
/// serve one that the router does not have.
fn after_help() -> String {
    format!(
        "Arguments are read from stdin as '<byte count>\\n<json>' and answers are\n\
         written to stdout the same way.  Start it as the agent's --sandbox command:\n\
         \n\
         \x20 ds4-agent --sandbox 'ds4-sandbox-helper --read-lines 240' -p 'prompt'\n\
         \n\
         Exit status: 0 when the agent closed the session, 1 when it stopped making\n\
         sense, 2 when these arguments could not be read.\n\
         \n\
         Tools served: {}",
        tools::SANDBOX_TOOLS.join(", ")
    )
}

fn main() -> ExitCode {
    // Help and version print and exit successfully; an argument that cannot be read
    // prints a usage line and exits 2, which is the status the help text promises.
    let config = Args::parse().config();

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    use clap::{CommandFactory, error::ErrorKind};

    /// Parsing looks at the environment as well as the command line, and tests run in
    /// parallel against one copy of the environment, so every test here takes this
    /// lock and holds it for as long as it is parsing or changing variables.
    static ENV: Mutex<()> = Mutex::new(());

    const READ_VAR: &str = "DS4_READ_LINES";
    const UPTO_VAR: &str = "DS4_EDIT_UPTO";

    /// `list` is the command line after the program name.
    ///
    /// The caller holds `ENV`, which is why it is passed in: holding it is the proof
    /// that nothing else in this process is reading the environment concurrently.
    fn config_from(list: &[&str], _env: &MutexGuard<'_, ()>) -> Result<Config, clap::Error> {
        Args::try_parse_from(std::iter::once(env!("CARGO_PKG_NAME")).chain(list.iter().copied()))
            .map(|args| args.config())
    }

    /// Runs `body` with neither variable set, and puts back what was there.  What the
    /// defaults are is only observable when the developer exporting them is not.
    ///
    /// # Safety
    /// Mutating the environment is unsound while another thread reads it, which is
    /// why the caller must hold `ENV`: every test in this module holds it too.
    unsafe fn without_env<T>(body: impl FnOnce() -> T) -> T {
        let saved = [READ_VAR, UPTO_VAR].map(std::env::var_os);
        for name in [READ_VAR, UPTO_VAR] {
            unsafe { std::env::remove_var(name) };
        }
        let out = body();
        for (name, value) in [READ_VAR, UPTO_VAR].into_iter().zip(saved) {
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
        out
    }

    #[test]
    fn the_defaults_are_the_conservative_ones() {
        let env = ENV.lock().unwrap();
        let config = unsafe { without_env(|| config_from(&[], &env)) }.unwrap();
        assert_eq!(config.read_lines, 120);
        assert!(!config.edit_upto);
    }

    #[test]
    fn both_spellings_of_an_option_with_a_value_work() {
        let env = ENV.lock().unwrap();
        assert_eq!(
            config_from(&["--read-lines", "240"], &env)
                .unwrap()
                .read_lines,
            240
        );
        assert_eq!(
            config_from(&["--read-lines=80"], &env).unwrap().read_lines,
            80
        );
        assert!(config_from(&["--edit-upto"], &env).unwrap().edit_upto);
        // The same switch can be turned back off on the command line, which is how
        // one turns off what the environment turned on.
        assert!(!config_from(&["--edit-upto=false"], &env).unwrap().edit_upto);
    }

    #[test]
    fn the_environment_sets_the_same_switches() {
        let env = ENV.lock().unwrap();
        unsafe {
            std::env::set_var(READ_VAR, "240");
            std::env::set_var(UPTO_VAR, "yes");
        }
        let config = config_from(&[], &env).unwrap();
        assert_eq!(config.read_lines, 240);
        assert!(config.edit_upto);
        // A command line outranks the environment, which is what makes the variable
        // a default rather than an override.
        assert_eq!(
            config_from(&["--read-lines", "60"], &env)
                .unwrap()
                .read_lines,
            60
        );
        unsafe {
            std::env::remove_var(READ_VAR);
            std::env::remove_var(UPTO_VAR);
        }
    }

    #[test]
    fn a_bad_value_is_refused_rather_than_ignored() {
        let env = ENV.lock().unwrap();
        for bad in [
            &["--read-lines", "wide"][..],
            &["--read-lines", "0"][..],
            &["--read-lines", "-3"][..],
            &["--read-lines"][..],
            &["--edit-upto", "maybe"][..],
            &["--nope"][..],
        ] {
            assert!(config_from(bad, &env).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn help_and_version_are_not_session_starts() {
        let _env = ENV.lock().unwrap();
        // Both are the parse failing with a request to print something, which is how
        // the binary exits successfully without reading a frame.
        for (flag, kind) in [
            ("--help", ErrorKind::DisplayHelp),
            ("-h", ErrorKind::DisplayHelp),
            ("--version", ErrorKind::DisplayVersion),
        ] {
            let err = Args::try_parse_from([env!("CARGO_PKG_NAME"), flag]).unwrap_err();
            assert_eq!(err.kind(), kind, "{flag}");
        }
    }

    #[test]
    fn the_help_names_the_options_and_the_served_tools() {
        let _env = ENV.lock().unwrap();
        let help = Args::command().render_help().to_string();
        for expected in ["--read-lines", "--edit-upto", "DS4_EDIT_UPTO"] {
            assert!(help.contains(expected), "help does not mention {expected}");
        }
        // The after-help is where the tool list lives, and it is built from the
        // router rather than typed twice.
        let long = Args::command().render_long_help().to_string();
        for tool in tools::SANDBOX_TOOLS {
            assert!(long.contains(tool), "help does not list {tool}");
        }
    }
}
