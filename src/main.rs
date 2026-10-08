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

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::ArgAction;
use clap::Parser;
use tools::BASH_SHELL;
use tools::Config;
use tools::FALLBACK_SHELL;

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

    /// The shell used to execute a command
    ///
    /// Commands requested by the agent are executed as  `<shell> -c <command>`. When
    /// not explicitly provided the helper uses `/bin/bash` or falls back to `/bin/sh`
    /// if not available.
    #[arg(long, value_name = "SHELL", env = "DS4_SHELL")]
    shell: Option<PathBuf>,

    /// Work in DIR instead of the directory this was started in
    ///
    /// The agent's own --chdir, for whoever starts this helper directly: every
    /// relative path in a request, and the directory `bash` commands begin in, is
    /// resolved there.  It happens before the first frame is read, so there is no
    /// moment when the answer to a relative path could come from somewhere else.
    ///
    /// There is no environment variable for it, unlike the options above.  A directory
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
            shell: self.shell.clone().unwrap_or_else(default_shell),
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

/// Whether `path` names something this process could exec: a file with an execute
/// bit, followed through a symlink because that is what exec does with one.
fn can_run(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The shell used to execute a command, using the `preferred` where this sandbox can
/// run it, and `fallback` where it cannot. This function only uses a `stat` rather than
/// a test run to determine if the given paths are valid.
fn pick_shell(preferred: &Path, fallback: &Path) -> PathBuf {
    if can_run(preferred) {
        preferred.to_path_buf()
    } else {
        fallback.to_path_buf()
    }
}

/// Select the from the default shell options based on availablity in the sandbox.
fn default_shell() -> PathBuf {
    pick_shell(Path::new(BASH_SHELL), Path::new(FALLBACK_SHELL))
}

/// Check whether or not the shell at the given path can run.
fn check_shell(shell: &Path) -> Result<(), String> {
    if shell.as_os_str().is_empty() {
        return Err("invalid shell: the name is empty".into());
    }
    if !shell.to_string_lossy().contains('/') {
        return Ok(());
    }
    match std::fs::metadata(shell) {
        Err(err) => Err(format!(
            "invalid shell {}: {}",
            shell.display(),
            files::err_message(&err)
        )),
        Ok(meta) if !meta.is_file() => Err(format!("{} is not a file", shell.display())),
        Ok(meta) if meta.permissions().mode() & 0o111 == 0 => {
            Err(format!("{} is not executable", shell.display()))
        }
        Ok(_) => Ok(()),
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
    if let Err(message) = check_shell(&config.shell) {
        eprintln!("ds4-sandbox-helper: {message}");
        return ExitCode::FAILURE;
    }

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
    const SHELL_VAR: &str = "DS4_SHELL";

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

    /// Runs `body` with none of the variables set, and puts back what was there.  What
    /// the defaults are is only observable when the developer exporting them is not.
    ///
    /// # Safety
    /// Mutating the environment is unsound while another thread reads it, which is
    /// why the caller must hold `ENV`: every test in this module holds it too.
    unsafe fn without_env<T>(body: impl FnOnce() -> T) -> T {
        const VARS: [&str; 3] = [READ_VAR, UPTO_VAR, SHELL_VAR];
        let saved = VARS.map(std::env::var_os);
        for name in VARS {
            unsafe { std::env::remove_var(name) };
        }
        let out = body();
        for (name, value) in VARS.into_iter().zip(saved) {
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

        assert_eq!(args.shell, None);
        let shell = args.config().shell;
        assert!(can_run(&shell), "{shell:?} cannot be run");
        assert!(
            shell == Path::new(BASH_SHELL) || shell == Path::new(FALLBACK_SHELL),
            "{shell:?} is neither of the two shells this knows about"
        );

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
            std::env::set_var(SHELL_VAR, "/bin/bash");
        }
        let config = config_from(&[], &env).unwrap();
        assert_eq!(config.read_lines, 240);
        assert!(config.edit_upto);
        assert_eq!(config.shell, PathBuf::from("/bin/bash"));
        // A command line outranks the environment, which is what makes the variable
        // a default rather than an override.
        assert_eq!(
            config_from(&["--read-lines", "60"], &env)
                .unwrap()
                .read_lines,
            60
        );
        assert_eq!(
            config_from(&["--shell", "/bin/zsh"], &env).unwrap().shell,
            PathBuf::from("/bin/zsh")
        );
        unsafe {
            std::env::remove_var(READ_VAR);
            std::env::remove_var(UPTO_VAR);
            std::env::remove_var(SHELL_VAR);
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
    fn a_shell_can_be_asked_for_in_either_spelling() {
        let env = ENV.lock().unwrap();
        for spelling in [&["--shell", "/bin/zsh"][..], &["--shell=/bin/zsh"][..]] {
            assert_eq!(
                config_from(spelling, &env).unwrap().shell,
                PathBuf::from("/bin/zsh"),
                "{spelling:?}"
            );
        }
        // A name without a slash is carried through as it was written: PATH is what
        // resolves it, at the moment the command is spawned.
        assert_eq!(
            config_from(&["--shell", "zsh"], &env).unwrap().shell,
            PathBuf::from("zsh")
        );
        // A flag that names no shell is a mistake in the command line, not a request
        // for the shell a run with no option would have settled on.
        assert!(config_from(&["--shell"], &env).is_err());
    }

    /// Test the process of resolving the default shell.
    #[test]
    fn the_shell_nobody_named_is_bash_where_bash_can_run() {
        let scratch = tempfile::TempDir::with_prefix("ds4-helper-pick-").unwrap();
        let bash = scratch.path().join("bash");
        let fallback = scratch.path().join("sh");
        std::fs::write(&bash, "#!/bin/sh\n").unwrap();
        std::fs::write(&fallback, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&fallback, std::fs::Permissions::from_mode(0o755)).unwrap();

        // There is a file called bash here and it is not something to run.
        assert_eq!(pick_shell(&bash, &fallback), fallback);

        std::fs::set_permissions(&bash, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(pick_shell(&bash, &fallback), bash);

        // A directory has execute bits of its own and is not a shell.
        assert_eq!(pick_shell(scratch.path(), &fallback), fallback);

        // The real look, in whichever direction this sandbox answers it.
        let chosen = default_shell();
        assert_eq!(
            chosen == Path::new(BASH_SHELL),
            can_run(Path::new(BASH_SHELL))
        );
    }

    /// The complaints `check_shell` makes, in its words: whoever reads one has to be
    /// able to tell "not there" from "not a file" and from "not executable".
    #[test]
    fn a_shell_that_cannot_run_is_named_before_anything_runs() {
        let scratch = tempfile::TempDir::with_prefix("ds4-helper-shell-").unwrap();

        let missing = scratch.path().join("missing");
        let err = check_shell(&missing).unwrap_err();
        assert!(
            err.starts_with(&format!("invalid shell {}: ", missing.display())),
            "{err}"
        );
        assert!(err.contains("No such file"), "{err}");

        let file = scratch.path().join("a-file");
        std::fs::write(&file, b"#!/bin/sh\n").unwrap();
        assert_eq!(
            check_shell(&file).unwrap_err(),
            format!("{} is not executable", file.display())
        );
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_shell(&file).is_ok());

        assert_eq!(
            check_shell(scratch.path()).unwrap_err(),
            format!("{} is not a file", scratch.path().display())
        );

        // A bare name is accepted whatever is on PATH, because the answer here would
        // be a second, differently obtained answer to the exec call's question.
        assert!(check_shell(Path::new("no-such-shell-anywhere")).is_ok());
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
        for expected in [
            "--read-lines",
            "--edit-upto",
            "--shell",
            "--chdir",
            "DS4_EDIT_UPTO",
            "DS4_SHELL",
        ] {
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
