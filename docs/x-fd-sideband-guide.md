# X Sideband File Descriptor Guide

This guide documents the fd lifecycle rules for x-thin Codex integrations that
launch Codex with sideband IO. The main failure mode is leaving an extra copy of
a pipe endpoint open in the wrong process. That prevents EOF, keeps readers or
writers alive forever, and makes managed Codex processes hard to stop cleanly.

The reference implementation is `c2c start codex` in
`~/src/c2c-msg` (`c2c_start.py` and the newer OCaml `ocaml/c2c_start.ml`).
That code may drift, but it shows the intended ownership shape.

## Sideband Fds

The x-thin Codex paths currently use these Unix-only inherited descriptors:

- `--xml-input-fd <fd>`: Codex/TUI or `codex-turn-start-bridge` reads
  XML-framed user messages from this fd.
- `--thread-id-fd <fd>`: `codex-turn-start-bridge` writes one thread handoff
  JSON line after thread start or resume.
- `--server-request-events-fd <fd>`: bridge writes server request events.
- `--server-request-responses-fd <fd>`: bridge reads responses to server
  request events.
- `--control-events-fd <fd>`: bridge writes unified control events, including
  `thread_resolved`.
- `--control-responses-fd <fd>`: bridge reads unified control responses.

Each flag names the fd number as seen by the Codex child after `exec`. Do not
reuse one fd number for multiple sideband flags. The bridge validates this, but
callers should still model each lane as a separate owned endpoint.

## Ownership Model

Treat every pipe endpoint as having exactly one long-lived owner:

- The Codex child owns the endpoints named in its `--*-fd` flags.
- The wrapper/supervisor owns the opposite endpoints.
- Sidecar delivery processes may temporarily receive an endpoint only if they
  become that endpoint's owner.
- Every process that is not an owner must close its copy immediately after
  `fork`, `dup2`, or `Popen`.

For `--xml-input-fd`, the usual ownership looks like this:

1. Parent creates a pipe.
2. Parent gives the read end to Codex at the fd number passed to
   `--xml-input-fd`.
3. Parent gives the write end to the broker delivery path, or keeps it if the
   parent itself writes XML.
4. Parent closes its original read end after spawning Codex.
5. Parent closes its original write end after spawning or handing off the writer.
6. The writer closes its write end when no more XML can arrive.
7. Codex receives EOF only after all writer copies are closed.

The important detail is that `dup2` and subprocess inheritance create additional
references to the same open file description. Closing "the pipe" in one place is
not enough if another inherited fd still points at the same endpoint.

## Correct Launch Pattern

When launching Codex from a Python supervisor:

```python
read_fd, write_fd = os.pipe()

def child_setup() -> None:
    os.dup2(read_fd, 3)
    if read_fd != 3:
        os.close(read_fd)
    os.close(write_fd)

proc = subprocess.Popen(
    ["codex", "--xml-input-fd", "3", *args],
    pass_fds=(read_fd,),
    preexec_fn=child_setup,
)
os.close(read_fd)

writer = subprocess.Popen(
    ["c2c", "deliver", "--xml-output-fd", str(write_fd)],
    pass_fds=(write_fd,),
)
os.close(write_fd)
```

The exact API may differ, but the structure should not:

- Make the sideband fd number explicit in the child command.
- Duplicate the inherited endpoint onto that exact fd in the child.
- Close the original fd in the child when it differs from the target number.
- Close the opposite endpoint in the child.
- Close the parent's copies after successful handoff.
- On launch failure, close both parent-side endpoints before returning or
  retrying.

The OCaml supervisor follows the same pattern manually with `Unix.pipe`,
`Unix.fork`, `Unix.dup2`, `Unix.execvpe`, and `Unix.close`.

## Rust Receiver Pattern

Codex sideband consumers should take ownership of inherited descriptors exactly
once. In Rust, converting an inherited fd with `File::from_raw_fd(fd)` transfers
close responsibility to that `File`; dropping the `File` closes the descriptor.

Use this shape for a reader or writer:

```rust
if fd < 0 {
    anyhow::bail!("invalid fd value `{fd}`");
}
let file = unsafe { std::fs::File::from_raw_fd(fd) };
```

After `from_raw_fd`, do not also call `libc::close(fd)`. That is a double close.
If code only needs to inspect or pass an fd without taking ownership, use a
borrowed fd API instead of `from_raw_fd`.

For async IO, wrap the owned file after conversion:

```rust
let file = unsafe { std::fs::File::from_raw_fd(fd) };
let file = tokio::fs::File::from_std(file);
```

For JSONL output lanes, flush after each line. Sideband clients often block on a
single event, so buffering without flushing looks like a dead bridge.

## Common Footguns

- Passing `--xml-input-fd 3` without ensuring fd 3 is open in the child.
- Passing the pipe's actual parent fd number in argv, then also `dup2`ing it to
  another number.
- Keeping the read end open in the parent after spawning Codex. This can keep
  the pipe alive after Codex exits and complicate restart cleanup.
- Keeping an extra write end open in the parent or a sidecar. Codex will not see
  EOF until every write fd is closed.
- Using `close_fds=False` or broad fd inheritance. Prefer close-on-exec by
  default and an explicit allowlist such as Python's `pass_fds`.
- Reusing one fd for both an event lane and a response lane. These are separate
  directions and should be separate descriptors.
- Calling `File::from_raw_fd` twice on the same fd in Rust. Only one owner may
  close a descriptor.
- Forgetting failure-path cleanup. If any spawn step fails, close every endpoint
  still owned by the current process before falling back or retrying.
- Assuming a subprocess that inherited a descriptor will close it promptly.
  Sidecars that daemonize or restart can accidentally keep endpoints open for
  the lifetime of the supervisor.

## Review Checklist

For every sideband fd change, check:

- Which process owns each endpoint after spawn?
- Which descriptors are passed through `exec`, and are all others close-on-exec?
- Does each `dup2` have a matching close of the old descriptor?
- Are unused opposite endpoints closed in the child before `exec`?
- Are parent copies closed after successful handoff?
- Are all endpoints closed on every error path?
- Will the reader observe EOF when the intended writer exits?
- Will the writer get `EPIPE` or `BrokenPipe` when the reader exits?
- Are JSONL event lanes flushed after each line?

If that ownership story is hard to state in one paragraph, simplify the launch
path before adding more fd plumbing.
