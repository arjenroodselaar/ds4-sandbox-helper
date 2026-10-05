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

use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::ArgAction;
use clap::Parser;
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

    /// Work in DIR instead of the directory this was started in
    ///
    /// The agent's own --chdir, for whoever starts this helper directly: every
    /// relative path in a request, and the directory `bash` commands begin in, is
    /// resolved there.  It happens before the first frame is read, so there is no
    /// moment when the answer to a relative path could come from somewhere else.
    ///
    /// There is no environment variable for it, unlike the two above.  A directory
    /// inherited through the environment would be entered twice over for a helper the
    /// agent had already moved, and a relative one would then mean somewhere else
    /// entirely.
    #[arg(long, value_name = "DIR")]
    chdir: Option<PathBuf>,
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

/// Moves the process into `dir`, checked and complained about in the same two steps
/// and the same words as `ds4-agent --chdir`: a launch that ended up somewhere else
/// would answer every later question about the wrong files, and nothing in a frame
/// would show it.  A directory the person named is not there is a mistake in the
/// command line, so it leaves with status 1 and no frame on stdout.
fn enter(dir: &Path) -> Result<(), String> {
    match std::fs::metadata(dir) {
        Err(err) => Err(format!(
            "invalid working directory {}: {}",
            dir.display(),
            files::err_message(&err)
        )),
        Ok(meta) if !meta.is_dir() => Err(format!("{} is not a directory", dir.display())),
        Ok(_) => std::env::set_current_dir(dir).map_err(|err| {
            format!(
                "failed to chdir to {}: {}",
                dir.display(),
                files::err_message(&err)
            )
        }),
    }
}

fn main() -> ExitCode {
    // Help and version print and exit successfully; an argument that cannot be read
    // prints a usage line and exits 2, which is the status the help text promises.
    let args = Args::parse();
    if let Some(dir) = &args.chdir
        && let Err(message) = enter(dir)
    {
        eprintln!("ds4-sandbox-helper: {message}");
        return ExitCode::FAILURE;
    }
    let config = args.config();

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
    use std::sync::Mutex;
    use std::sync::MutexGuard;

    use clap::CommandFactory;
    use clap::error::ErrorKind;

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
    fn args_from(list: &[&str], _env: &MutexGuard<'_, ()>) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once(env!("CARGO_PKG_NAME")).chain(list.iter().copied()))
    }

    fn config_from(list: &[&str], env: &MutexGuard<'_, ()>) -> Result<Config, clap::Error> {
        args_from(list, env).map(|args| args.config())
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
        let args = unsafe { without_env(|| args_from(&[], &env)) }.unwrap();
        assert_eq!(args.read_lines, 120);
        assert!(!args.edit_upto);
        // No directory asked for means the one the process was started in, which is
        // the only answer that does not need this program to have an opinion.
        assert_eq!(args.chdir, None);
    }

    #[test]
    fn a_directory_can_be_asked_for_in_either_spelling() {
        let env = ENV.lock().unwrap();
        for spelling in [&["--chdir", "/src"][..], &["--chdir=/src"][..]] {
            let args = args_from(spelling, &env).unwrap();
            assert_eq!(
                args.chdir.as_deref(),
                Some(Path::new("/src")),
                "{spelling:?}"
            );
        }
        // A flag that names no directory is a mistake in the command line, not a
        // request to stay where the launcher happened to be.
        assert!(args_from(&["--chdir"], &env).is_err());
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

    /// The two complaints `ds4-agent --chdir` makes, in its words: a person reading
    /// them has to be able to tell "not there" from "not a directory".
    ///
    /// Only the failures are tried here.  Succeeding would move the working directory
    /// of this whole test process, and the other tests in it are running in parallel
    /// with paths of their own; the success path is what the end-to-end test does in a
    /// process of its own.
    #[test]
    fn a_directory_that_cannot_be_worked_in_is_named_before_anything_runs() {
        let scratch = tempfile::TempDir::with_prefix("ds4-helper-chdir-").unwrap();

        let missing = scratch.path().join("missing");
        let err = enter(&missing).unwrap_err();
        assert!(
            err.starts_with(&format!(
                "invalid working directory {}: ",
                missing.display()
            )),
            "{err}"
        );
        assert!(err.contains("No such file"), "{err}");

        let file = scratch.path().join("a-file");
        std::fs::write(&file, b"not a directory\n").unwrap();
        assert_eq!(
            enter(&file).unwrap_err(),
            format!("{} is not a directory", file.display())
        );
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
        for expected in ["--read-lines", "--edit-upto", "--chdir", "DS4_EDIT_UPTO"] {
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
