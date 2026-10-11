# Why — the workspace registry, the project file, and a session's own state

## Which file holds what is a security boundary, not a layout choice

- `~/.config/geli/workspaces/<name>.json` — `{"dirs": [...], "ssh": bool, "allow": [...]}`. The
  user's decisions, out of the sandbox's reach. `resolve_workspace` appends the current directory;
  stale paths are filtered at read time rather than pruned, because a directory on an unmounted
  volume is not the same thing as one that was removed.
- `./.geli.json` in each project dir — `{"packages": {...}}` and nothing else. The project's
  *declaration*: untrusted, validated by `rejected_package_names`, and committable, which is what
  `packages` wanted all along — the tools a project needs are a fact about the repository.

`.geli.json` lives in a directory the sandbox mounts **with write access**, so its contents are the
agent's output as much as the user's input. It therefore grants nothing. Before the split, a
`{"ssh": true}` in that file forwarded the host's private SSH keys into the VM with no flag — so the
agent in one session could grant itself keys for the next, and so could a cloned repository carrying
the field. `allow` widened the session's own egress allowlist, and `workspace` reassigned the
directory, putting another workspace's directories in the mount set.

## The binding moves too, and that is the part with a visible cost

`workspace_for_dir` finds the directory by asking which workspace already lists it, and a directory
in none of them is a first run. A clone therefore gets asked once, where before it arrived knowing.
That convenience *was* the hole: whoever writes in the repository decides the assignment, and inside
the VM that is the agent.

`migrate_local_config` moves `workspace` and `allow` out of an older file and deletes it when
nothing is left. **It does not move `ssh`**; it reports it and drops it, because importing that
field from a file the agent could have written would bless the agent's decision as the host's. The
user is told to run `geli --ssh` once if they want it back.

## The registry's `.txt` format was dropped, not read alongside

geli had one user when the format changed, so every one-path-per-line registry in existence was on
one machine and was deleted by hand. No reader, and no line telling anyone to clean up — both would
be transition code written for nobody, which is how a codebase ends up carrying a decade of other
people's migrations. Re-registering a directory is one prompt.

## Workspace names are sanitized wherever they come from

They become filenames and the guest hostname, so they are reduced to lowercase alphanumerics plus
`-`. Only the name typed at the prompt used to get that treatment; the one read out of `.geli.json`
went straight into the path the registry file is written to, where `../` leaves it.
`sanitize_workspace_name` is the single rule, a name that survives nothing is no name at all, and a
name that had to be changed says so on stderr.

## `.geli.json` is read once, and a malformed one is an error

Three separate readers each parsed the file again for one field, and each swallowed a parse failure
into a default while `get_or_create_workspace` — reading the same file — raised it. So a typo
silently dropped the project's packages, and then the writer overwrote the unparseable file with
that default and took the rest of its contents with it. `read_local_config` returns `Ok(None)` only
for a file that is genuinely absent.

## The user's arguments keep their quoting

Joining with a bare space made `geli claude -p "explain this"` arrive as four arguments, because the
guest's shell split the line again. Quoting each argument fixes that, but the joined string had a
second job: `agent_for_command` matched a recipe against its first word, and `'claude'` matches
nothing — the session would have run with **no credentials**, saying only that the command is not an
agent. A silent fall to a worse mode is the one outcome worth designing against.

So the two forms are two fields. `GuestCommand` holds `line` (quoted, for the guest's shell) and
`program` (raw, for matching), and the matching helpers take the program so the quoted form cannot
reach them by accident — which is what the type is for.

One consequence: `geli claude $FOO` no longer expands in the guest. The shell that used to do that
expansion was the one splitting the arguments.

## `SessionScratch`, and why `Drop` is only half of it

The overlay and `/tmp/sandbox-share-<pid>` used to be deleted by the last two lines of
`execute_sandbox`, so any of the two dozen `?`s before them left both behind — and a failure is
exactly when the files are large. The guard deletes them however the function leaves, honouring
`GELI_KEEP`.

**But `Drop` does not run on `process::exit`**, so the other half is that nothing between creating
those files and the end of the session exits the process: the one call that did
(`collect_ssh_credentials` failing) now returns an error. A guard with an `exit` still live
underneath reads as if cleanup were handled and is worse than none.

`main` is a wrapper that prints the error and exits, which is safe because every guard has dropped
by the time `run` returns — and it prints the sentence rather than `Result`'s `Debug`, which renders
an `io::Error` as `Custom { kind: Other, error: "..." }`.

## What a session leaves for the host to run

`.git/hooks`, `.git/config`, `.envrc` and `.vscode/tasks.json` are inside the project, so the agent
was given them — and the *host* runs them afterwards, outside any sandbox: `git commit` fires a
hook, `cd` fires `.envrc`, `core.hooksPath` and `core.fsmonitor` do it a level of indirection away.
The threat model ("the agent cannot touch what it was not given") does not cover this, and cannot,
because a project the agent cannot write to is a project it cannot work on.

So this does not prevent it, it says it. `fingerprint_host_executed` hashes each watched file before
the VM launches and again after it exits; `changed_paths` reports what was added, edited or removed.
A session that touched nothing says nothing.

Mounting `.git/hooks` read-only was considered and rejected: it breaks `git commit` in every
repository with husky or pre-commit installed, which is most of them, and it closes one of six
routes.

**The comparison and the line both come before `offer_to_persist_packages`**, because that prompt is
the last thing a session does and a user who leaves it with Ctrl-C must not take the warning with
them. **Nothing geli writes itself may join the watched set**, or every session that persists a
package would end by accusing itself — there is a test.

## The pure parts of `execute_sandbox` live in the module they belong to

`share_args` (`qemu.rs`) builds the `-fsdev`/`-device` pairs; the tags are positional, so the order
of `dirs` is part of the contract. `status_mounts` (`ui.rs`) decides what the status block claims is
mounted — the only place the user is told what the sandbox can see, so a wrong line there is a wrong
belief about what the agent was given. `session_warnings` (`agents.rs`) gathers everything geli says
before the VM boots, and the order is part of it: what shapes the session before what is odd about
the command.

All three were inline, which meant the only way to check that `--restrict-net --ssh` warns about
SSH, or that the right directory is marked active, was to boot a VM and look.
