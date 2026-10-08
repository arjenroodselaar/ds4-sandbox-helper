# ds4-sandbox-helper

The sandbox helper for [`ds4-agent --sandbox`](../ds4/docs/SANDBOX.md).
`ds4-agent` can run with no filesystem or shell access of its own: it sends
every tool call over a pair of pipes and this program answers them. What the
agent may touch is then whatever this program runs inside — a container, a VM, a
chroot, a remote host — and the agent binary itself never needs the paths
involved to exist.

It answers the framed protocol on stdin and stdout, and writes nothing but
frames to stdout. Diagnostics, and the output of the commands it runs, go to
stderr or to files.

## Build and run

```sh
cargo build --release
../ds4/ds4-agent --sandbox ./target/release/ds4-sandbox-helper -p "…"
```

`--sandbox` takes a command line, so anything that speaks the protocol works:

```sh
ds4-agent --sandbox 'docker run -i --rm -v "$PWD:/w" -w /w helper' …
```

## Startup

The first frame is the helper's own, written before a byte of a request is read:

```
149
{"id":0,"text":"ds4-sandbox-helper 0.1.2 ready: dir /home/ds4-sandbox-helper, shell /bin/bash, read_lines default 120, upto marker off","type":"log"}
```

The word `ready` is what the agent blocks on, and it blocks before loading a
model. The wait ends when that word arrives, when the process dies, or when it
sends something that is not a frame. Everything else in the line is for the user
and the agent prints it as `sandbox: …` beside its own log messages. So the log
reports which sandbox is answering, in which directory, with which shell and
which settings.

Until that notice arrives the agent also echoes the helper's stderr a line at a
time with the same `sandbox: ` prefix, which is how a bad option or a directory
that does not exist reaches the terminal instead of a bare exit code. After it,
stderr is diagnostics again: kept as a tail, mirrored to the agent's trace, and
reported when the sandbox dies. Printing to stdout outside a frame is a protocol
fault, hello included, and a startup that never reaches the notice leaves the
agent waiting for as long as the process lives.

## Options

| Option           | Env              | Default                     |
| ---------------- | ---------------- | --------------------------- |
| `--read-lines N` | `DS4_READ_LINES` | `120`                       |
| `--edit-upto`    | `DS4_EDIT_UPTO`  | off                         |
| `--shell SHELL`  | `DS4_SHELL`      | `/bin/bash`, else `/bin/sh` |
| `--chdir DIR`    | —                | the launch directory        |

`--read-lines N` is the fallback for how many lines a bare `read` or `more`
returns. The agent sends the real number with every request
(`limits.read_lines`), because it is the one that knows the model's context
size; this is what is used when a request does not say, which in practice means
a person at a terminal.

`--edit-upto` accepts an `[upto]` marker in `edit`'s `old` text, which selects
everything between two anchors. It can delete a great deal at once, so it is
opt-in.

`--shell SHELL` is the shell used to execute a command, as
`<shell> -c <command>`. When not explicitly provided the helper uses `/bin/bash`
if available in the sandbox, so arrays, `[[ ]]` and process substitution mean
what the model meant. If not available the helper falls back to `/bin/sh` which
is assumed to be available in the sandbox. The sandbox startup notice reports
which shell has been selected. An absolute path is checked before the first
frame with errors reported on stderr and status 1 . A path without a slash is
left for `PATH` to resolve when the command runs; checking it here would mean
answering the same question twice, and a wrong answer refuses a shell that
works.

`--chdir DIR` works in `DIR` instead: every relative path in a request, and the
directory `bash` starts a command in, resolve there. It happens before the first
frame is read, so nothing is ever answered from somewhere else. The same flag
and the same complaints as `ds4-agent --chdir`. No environment variable: a
directory inherited through one would be entered twice over for a helper the
agent had already moved, and a relative one would then mean somewhere else
entirely.

The first three also read the matching environment variable, which is easier
than quoting them through a container's `-e`. `--help` lists all four with their
defaults, and a value that does not parse — on the command line or in the
environment — stops the helper with usage on stderr rather than being quietly
ignored. A `--chdir` that cannot be done and a `--shell` that cannot be run are
a different kind of mistake, and get the agent's own words for it:
`invalid working directory …`, `… is not a directory`, `invalid shell …`,
`… is not a file`, `… is not executable`, on stderr, with status 1 and nothing
on stdout.

## Tools

`read`, `more`, `search`, `list`, `write`, `edit`, `bash`, `bash_status`,
`bash_stop`.

The answers are the same text the agent produces when it runs these tools
itself, line numbers and all: a model should not have to know which side of the
pipe it is talking to. `bash` keeps jobs running in the background the way the
agent does, with the same `bash_status` and `bash_stop` follow-ups, and a
command that will not stop when told is killed along with its process group at
the end of its deadline. A job also outlives neither the session nor its own
group: when the run ends, by a closed stdin or by the agent's teardown signal,
whatever is still running is asked to stop, given a moment, and then killed
along with its group, because the agent's signal stops at this process's group
and a command started here runs in another one. `refresh_sec` on all three is
how long the helper is allowed to take, not how long it takes: the answer goes
out the moment the command finishes, and only a command still running at the
deadline waits that long. A job finished by a signal reports `exit_status` as
128+signal, so a stopped job reads as 143 or 137.

A request carries with it the caps the helper cannot work out for itself, in a
`limits` object of its own. The one today is `limits.read_lines`, the size a
bare `read` or `more` should return: it follows the model's context window, and
is capped at 500 here whatever the sender asks for. A limit this version does
not know is ignored rather than rejected, which is the point of keeping them in
an object of their own: another cap can be named later without changing the
shape of a request. `--read-lines` is what a bare read means when the sender
says nothing, which in practice means a person at a terminal.

There is no path restriction in here. The helper does what it is asked, because
it is meant to be the thing that runs inside a boundary that is enforced
elsewhere. Putting a path jail in the helper would let a compromised agent talk
its way out of it, which is the one thing the design has to avoid.

## Layout

- `src/wire.rs` — The frame: a decimal byte count, a newline, that many bytes.
- `src/protocol.rs` — Request and response shapes, notices, argument extraction.
- `src/budget.rs` — The 128 KiB output ceiling every answer is cut to.
- `src/files.rs` — Reading, and replacing a file without following a symlink or
  losing its mode.
- `src/tools/` — One module per tool, plus the dispatch.
- `src/server.rs` — The read–dispatch–answer loop.
- `tests/e2e.rs` — The real binary over real pipes, real files and real
  processes.

## One runtime

Everything is async: the frame loop, the tools, the commands they start. The two
things that genuinely cannot be async run on the blocking pool instead of the
task — replacing a file, which is one sequence of open, temporary, copy of
ownership and rename that has to stay a unit, and the odd open that needs flags
Tokio's own options do not expose. Nothing blocks the runtime itself, which is
what keeps a long `bash` job, a slow disk and a big `search` from holding each
other up.

## Temporary files

Two files are made on the fly: the one an `edit` or a `write` is replaced
through, and the one a `bash` command's output is spooled into. Both come from
`tempfile`, and both keep the names the C agent gives its own —
`<name>.ds4-XXXXXX` beside the file being replaced, `ds4_agent_output_XXXXXX` in
the temp directory — so a leftover from either program says the same thing to
whoever finds it. `tempfile` also decides what becomes of them: a replace
temporary that never reached its rename deletes itself, while the spool file is
handed over to the job, because the model is shown its path and reads it back
after the command is gone.

## How much of this is a port

The behaviour is a faithful re-reading of the tool implementations in
`ds4_agent.c`, including the wording of the messages, which matters because the
model sees them and has learned what they mean. Where the C code and the spec
doc disagree, the C code wins: it is what models have been tuned against, and it
is what the tests in this repo are checked against.

Idiomatic Rust is used wherever it does not change an answer. Two places are
worth naming because they are the sort of thing that would otherwise be a
surprise:

- The `regex` crate is not POSIX ERE. Backreferences do not exist in it, and a
  pattern that uses them comes back as `invalid regex: …` rather than matching
  literally. Everything the agent's own regex mode does in practice is
  supported.
- Answers that hit the byte ceiling carry a note saying the output was
  truncated. The agent cuts its own tool output silently, because it knows the
  model can ask again; a helper that cut silently would leave a model wondering
  whether the file simply ended there. `read` does not add the note, since it
  already ends with the offsets to resume from.

## Platforms

macOS and Linux are both supported, and they differ in one place worth naming:
the metadata that comes across when a file is replaced. macOS has a call that
copies the rest of a file in one go, and it is used for the ACL and the extended
attributes. Linux has no such call, but it keeps the POSIX ACL as an attribute
of its own (`system.posix_acl_access`), so carrying the attributes across
carries the ACL with them. Both are best effort: a label this process may not
write — an SELinux context, a `trusted.*` attribute — is skipped, and the write
still succeeds. A filesystem that will not hold attributes at all is not a
failure of the write either, and the test that checks this quietly does nothing
there.

## Tests

```sh
cargo test
```

The unit tests cover the byte-level cases that are easy to get wrong and hard to
see: a line that stops mid-character, a resume offset in the middle of a line, a
file that changes between read and write. The integration tests start the built
binary and talk to it the way the agent does, which is the only way to catch a
frame whose length is off by one.

To check the two implementations agree, run the C suite against this binary:

```sh
cargo build --release
HELPER=$PWD/target/release/ds4-sandbox-helper
cd ../ds4 && DS4_SANDBOX_HELPER=$HELPER ./ds4_agent_test
```
