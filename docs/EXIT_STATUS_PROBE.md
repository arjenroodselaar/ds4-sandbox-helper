# `exit_status=` sometimes absent from a bash answer in the model's view

Observed once: a `status=done` bash answer reached the model without the
`exit_status=` line that belongs under the header.  The helper cannot produce
that shape.  `observation` writes the header and the line from the same
`Status::Done` arm (`src/tools/bash.rs:379-400`), and 50 recorded raw frames
from two builds carried it on every done answer, with signal deaths reported as
`128 + signal` like the native tool does.

## If it appears again, while the transcript is still fresh

1. Quote the answer's first three lines verbatim in the reply text, including
   the `job=` number.  A paraphrase cannot be debugged.
2. Say which region the answer sits in.  Above
   `[End compacted summary. Recent conversation continues verbatim below.]` is
   the summary, below it the tail.  The tail is copied token for token
   (`ds4_agent.c:11113`), so a line missing there is a real loss, while a line
   missing from the summary only means the model left it out.
3. Re-run the same command as a new call.  Do not use `bash_status` on the old
   job: a job is dropped once observed done, so that answers
   `bash job not found`.
4. Get the wire truth with the driver below, same command.
5. Ask for the agent-side artefacts.  They live on the agent host, which the
   sandbox cannot read: the git rev of `/home/ds4` there, whether the run used
   `--trace`, and a grep of `compacted_transcript` and `compaction-summary` for
   `exit_status=`.

## What each combination means

- Frame has it, tail has it: nothing to chase.
- Frame has it, tail lacks it: the loss is between the frame and the transcript.
  Look at the verbatim forward in `agent_sandbox_tool_call` (`ds4_agent.c:10181`)
  and the `tool`-role append (`ds4_agent.c:10688`).
- Frame has it, only the summary lacks it: expected, compaction is lossy and
  the tail is verbatim.
- Frame lacks it: helper bug.  `Status` has two states and the line is written
  in one of them, so look for a status that is neither or for an answer built
  outside `observation`.

## Ground truth from the helper, without the agent in the path

    python3 probe_frames.py target/release/ds4-sandbox-helper /tmp 'exit 0' 'exit 3' 'kill -TERM $$'

Expects `exit_status=0`, `exit_status=3`, `exit_status=143`, and a final count
of done answers without the line.

## Run A, done after a fresh agent and sandbox start

One tool call per turn, in this order, nothing before step 1.  Steps 1 and 2 are
the same command, so only the position differs.  Report each as a PROBE line
quoting the first two lines exactly.

1. `bash` `exit 0`
2. `bash` `exit 0`
3. `bash` `exit 3`
4. `bash` `exit 0`
5. `bash` `kill -TERM $$`

## Result of run A

Not reproduced.  All five answers carried the line, values `0`, `0`, `3`, `0`
and `143`, and the first was `job=1`, so nothing bash ran before it.  The
line-less answers quoted earlier all came out of a compacted context, which is
a plausible explanation only for the ones that sat in the summary region.  Run B
was not needed.

## Run B, only if a fresh uncompacted first call loses the line

Restart agent and sandbox, then:

1. `read` of `Cargo.toml`
2. `bash` `exit 0`

If step 2 loses the line the trigger is the first bash-shaped observation the
client renders.  If it keeps it the trigger is the first request the client
sends, whatever tool it names.

## Facts worth not rediscovering

- `bash` waits up to `refresh_sec`, default 60 seconds, so a short command
  answers done in the call that started it.
- `exit_status_of` (`src/tools/bash.rs:346`) maps a signalled child to
  `128 + signal` and an unwaitable one to `-1`, matching `ds4_agent.c:8886`.
- The agent's `status=done` without `exit_status=0` test (`ds4_agent.c:9336`)
  colours the user's terminal.  It is not the transcript.
- A done answer always carries the line and a running answer never does, so the
  header alone is a reliable check on any recorded answer.
