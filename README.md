# ds4-sandbox-helper

The sandbox helper for [`ds4-agent --sandbox`](../ds4/docs/SANDBOX.md). `ds4-agent`
can run with no filesystem or shell access of its own: it sends every tool call over
a pair of pipes and this program answers them. What the agent may touch is then
whatever this program runs inside — a container, a VM, a chroot, a remote host — and
the agent binary itself never needs the paths involved to exist.

It answers the framed protocol on stdin and stdout, and writes nothing but frames to
stdout. Diagnostics, and the output of the commands it runs, go to stderr or to files.

## Build and run

```sh
cargo build --release
../ds4/ds4-agent --sandbox ./target/release/ds4-sandbox-helper -p "…"
```

`--sandbox` takes a command line, so anything that speaks the protocol works:

```sh
ds4-agent --sandbox 'docker run -i --rm -v "$PWD:/w" -w /w helper' …
```

## Options

Both also read the matching environment variable, which is easier than quoting them
through a container's `-e`.  `--help` lists the same two, with their defaults, and a
value that does not parse — on the command line or in the environment — stops the
helper with usage on stderr rather than being quietly ignored.

| Option | Env | Default | Why it exists |
| --- | --- | --- | --- |
| `--read-lines N` | `DS4_READ_LINES` | `120` | Fallback for how many lines a bare `read` or `more` returns. The agent sends the real number with every request (`limits.read_lines`), because it is the one that knows the model's context size; this is what is used when a request does not say, which in practice means a person at a terminal. |
| `--edit-upto` | `DS4_EDIT_UPTO` | off | Accepts an `[upto]` marker in `edit`'s `old` text, which selects everything between two anchors. It can delete a great deal at once, so it is opt-in. |

## Tools

`read`, `more`, `search`, `list`, `write`, `edit`, `bash`, `bash_status`, `bash_stop`.

The answers are the same text the agent produces when it runs these tools itself, line
numbers and all: a model should not have to know which side of the pipe it is talking
to. `bash` keeps jobs running in the background the way the agent does, with the same
`bash_status` and `bash_stop` follow-ups, and a command that will not stop when told is
killed along with its process group at the end of its deadline. `refresh_sec` on all
three is how long the helper is allowed to take, not how long it takes: the answer goes
out the moment the command finishes, and only a command still running at the deadline
waits that long. A job finished by a signal reports `exit_status` as 128+signal, so a
stopped job reads as 143 or 137.

A request carries with it the caps the helper cannot work out for itself, in a
`limits` object of its own.  The one today is `limits.read_lines`, the size a bare
`read` or `more` should return: it follows the model's context window, and is capped
at 500 here whatever the sender asks for.  A limit this version does not know is
ignored rather than rejected, which is the point of keeping them in an object of
their own: another cap can be named later without changing the shape of a request.
`--read-lines` is what a bare read means when the sender says nothing, which in
practice means a person at a terminal.

There is no path restriction in here. The helper does what it is asked, because it is
meant to be the thing that runs inside a boundary that is enforced elsewhere. Putting a
path jail in the helper would let a compromised agent talk its way out of it, which is
the one thing the design has to avoid.

## Layout

| File | Contents |
| --- | --- |
| `src/wire.rs` | The frame: a decimal byte count, a newline, that many bytes. |
| `src/protocol.rs` | Request and response shapes, notices, argument extraction. |
| `src/budget.rs` | The 128 KiB output ceiling every answer is cut to. |
| `src/files.rs` | Reading, and replacing a file without following a symlink or losing its mode. |
| `src/tools/` | One module per tool, plus the dispatch. |
| `src/server.rs` | The read–dispatch–answer loop. |
| `tests/e2e.rs` | The real binary over real pipes, real files and real processes. |

## One runtime

Everything is async: the frame loop, the tools, the commands they start. The two
things that genuinely cannot be async run on the blocking pool instead of the task —
replacing a file, which is one sequence of open, temporary, copy of ownership and
rename that has to stay a unit, and the odd open that needs flags Tokio's own options
do not expose. Nothing blocks the runtime itself, which is what keeps a long `bash`
job, a slow disk and a big `search` from holding each other up.

## How much of this is a port

The behaviour is a faithful re-reading of the tool implementations in `ds4_agent.c`,
including the wording of the messages, which matters because the model sees them and
has learned what they mean. Where the C code and the spec doc disagree, the C code
wins: it is what models have been tuned against, and it is what the tests in this repo
are checked against.

Idiomatic Rust is used wherever it does not change an answer. Two places are worth
naming because they are the sort of thing that would otherwise be a surprise:

- The `regex` crate is not POSIX ERE. Backreferences do not exist in it, and a pattern
  that uses them comes back as `invalid regex: …` rather than matching literally.
  Everything the agent's own regex mode does in practice is supported.
- Answers that hit the byte ceiling carry a note saying the output was truncated. The
  agent cuts its own tool output silently, because it knows the model can ask again;
  a helper that cut silently would leave a model wondering whether the file simply
  ended there. `read` does not add the note, since it already ends with the offsets to
  resume from.

## Tests

```sh
cargo test
```

The unit tests cover the byte-level cases that are easy to get wrong and hard to see:
a line that stops mid-character, a resume offset in the middle of a line, a file that
changes between read and write. The integration tests start the built binary and talk
to it the way the agent does, which is the only way to catch a frame whose length is
off by one.

To check the two implementations agree, run the C suite against this binary:

```sh
cargo build --release
cd ../ds4 && DS4_SANDBOX_HELPER=$PWD/../ds4-sandbox-helper/target/release/ds4-sandbox-helper ./ds4_agent_test
```
