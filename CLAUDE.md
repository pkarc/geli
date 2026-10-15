# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

**This file holds the rules. `docs/why/` holds how each was arrived at** — the measurements, and the
sequence of failures that produced them. A rule here should be enough to follow; read the linked
note before changing one, because most of them look arbitrary until you know what they cost.

## What this is

`geli` is a Rust CLI that runs an AI coding agent inside a disposable QEMU virtual machine. Isolation is at the hardware/VM level, not containers or namespaces — the agent gets a fresh Alpine cloud image each invocation, with only the registered project directories mounted in over 9p.

Usage: `geli <command> [args...]` runs `<command>` inside the sandbox; `geli --list` prints the workspace registry.

## Build / run

```bash
cargo build                    # debug build
cargo build --release          # release build
cargo test                     # unit tests, all on pure functions
cargo test base_cloud_init     # single test
cargo run -- --list            # run without installing
cargo run -- --build-image     # (re)build the base image; 5-20 min, mostly downloads
cargo run -- --build-image --agents claude   # and that agent's layer, instead of on first use
./setup.sh                     # host deps + cloud image + binary + base image
```

Recipes live in `agents/*.toml` and are discovered by `build.rs`, so touching one triggers a
rebuild without any Rust changing.

`cargo test dump_generated_documents -- --ignored` writes every generated guest document to
`/tmp/geli-baseline`. **Dump before a refactor, dump after, and diff** — that is the proof a change
to the guest's configuration changed nothing.

`GELI_KEEP=1 geli <cmd>` preserves the session qcow2 and `/tmp/sandbox-share-<pid>/` instead of
deleting them on exit — the only way to inspect `user-data` or the guest's
`/var/log/cloud-init-output.log` after a failed boot.

**Do not run `cargo fmt`, and `cargo fmt --check` is deliberately not a gate.** The layout is
deliberate and rustfmt cannot tell a meaningful line break from an accidental one. Match the
surrounding code by hand. → [`docs/why/build-and-ci.md`](docs/why/build-and-ci.md)

CI is two workflows: **`check`** (build, test, clippy `-D warnings`, no VM) on every push and PR,
and **`image`** (boots real VMs, builds the base and the layers a PR touched) on recipe changes and
weekly. Only `image` proves a recipe *installs*, because cloud-init does not abort on failure.
→ [`docs/why/build-and-ci.md`](docs/why/build-and-ci.md)

**A recipe's `install` is arbitrary root shell, and `image` runs it.** The workflow uses
`pull_request` and never `pull_request_target`, and GitHub's "require approval for all outside
collaborators" setting must stay on. → [`docs/why/build-and-ci.md`](docs/why/build-and-ci.md)

Running the sandbox requires, on the host: `qemu-system-x86_64`, `qemu-img`, `genisoimage`, KVM access, and the base image at `~/qemu-sandbox/nocloud_alpine-3.22.2-x86_64-bios-cloudinit-r0.qcow2`. `setup.sh` provisions all of these for Ubuntu/Debian hosts. `GELI_BASE_IMAGE` points `--build-image` at a different cloud image. `main.rs` re-checks qemu/genisoimage at runtime and exits with install instructions if missing.

## Architecture

```
src/main.rs     CLI, the workspace registry, the local config, orchestration
src/agents.rs   the agents geli knows, parsed from agents/*.toml
src/guest.rs    what the guest is told to be: cloud-init, recipes, mounts, boot
src/net.rs      egress policy: the allowlist and the proxy that enforces it
src/ssh_agent.rs  the agent relay: what the guest may ask the host's key to do
src/qemu.rs     the Linux driver: images on disk, direct boot, the boot watch
src/ui.rs       the status block and the loading line
agents/*.toml   one recipe per agent
src/guest/*.sh  the recipes themselves, as real shell and YAML
```

**Module boundaries are by subject, not convenience, and every module holds its own tests.** If
something feels misfiled it probably is — move it rather than leaving a note. After moving tests,
check the count: a `#[test]` in a module that is not declared does not fail, it disappears.
→ [`docs/why/build-and-ci.md`](docs/why/build-and-ci.md)

Recipes are files with `@NAME@` placeholders, not Rust strings, because every `{` in a shell script
had to be doubled to survive `format!`.

### Workspace registry and the project file

A "workspace" is a named group of directories that should see each other inside one VM. **Which
file holds what is a security boundary, not a layout choice:**

- `~/.config/geli/workspaces/<name>.json` — `{"dirs": [...], "ssh": bool, "allow": [...]}`. The
  user's decisions, out of the sandbox's reach.
- `./.geli.json` in each project dir — `{"packages": {...}}` and nothing else. Untrusted, validated
  by `rejected_package_names`, and committable.

So invoking `geli` from directory A in workspace `foo` mounts *every* directory registered under
`foo`, with A as the active one the command `cd`s into.

**Nothing in `.geli.json` may grant anything** — the sandbox mounts it with write access, so the
agent can write it for the next session. The binding lives in the registry (`workspace_for_dir`
looks it up), `migrate_local_config` moves `workspace` and `allow` out of older files and
deliberately does *not* move `ssh`, and `<name>.json` is the only registry format the code knows.
Workspace names are sanitized wherever they come from, and `.geli.json` is read once into a
`LocalConfig` where a malformed file is an error.
→ [`docs/why/workspaces-and-session.md`](docs/why/workspaces-and-session.md)

**`packages` is the one thing in that file that acts, so it is gated twice and placed carefully.**
A name must be a package and not a *place*, an *option* or an *archive* — `npm install -g ./evil`
runs that directory's install script, before the lockdown, and no shell quoting stops a path.
**Only apk installs as root** (Alpine signs it, and no recipe or `--allow-untrusted` gets past the
validator); npm and pip run as `sandbox` into `/home/sandbox/.local`, which is on the exported
`PATH`. **And they run before the workspace is mounted and before the credentials are installed**,
so an install script has no project to read and no token to take — the order in
`build_mount_script` is the security model, and a test asserts it as an order. The rules are per manager,
because only npm needs `@scope/name`. And **the list lives in the project while the approval lives
in the registry**: a changed list is asked about once, and `approved_packages` is keyed by
directory so a project cannot approve itself. Not a terminal means not installing.
→ [`docs/plans/10-paquetes-token-y-lo-que-no-se-puede-comprobar.md`](docs/plans/10-paquetes-token-y-lo-que-no-se-puede-comprobar.md)

### Guest configuration

`build_mount_script`, `build_cloud_init`, `build_base_cloud_init`, `build_layer_cloud_init`,
`build_env_exports` and `indent_block` turn a workspace into a cloud-init document with no I/O.
**Keep them pure** — that is what makes the guest config testable without booting a VM. Add tests
there rather than debugging through the serial console.

**There are three cloud-init documents, and the split is the whole performance story.** The base is
baked once (toolchain, user, autologin, `LOGIN_PROFILE`), a layer is baked once per agent (that
agent's install), and the session document holds only what depends on the workspace. **Anything
slow belongs in one of the first two; anything shared belongs in the base.**
→ [`docs/why/guest-config.md`](docs/why/guest-config.md)

**The cloud-init YAML has a deliberately fixed shape.** Anything variable goes in a `content: |`
block via `indent_block`, never in a list item — bad indentation fails *silently*, and the VM boots
fine with the command never running. Extend `cloud_init_parses` when you add one.
→ [`docs/why/guest-config.md`](docs/why/guest-config.md)

### The Linux session

`execute_sandbox` is `#[cfg]`-gated per OS; only Linux exists, macOS and Windows are stubs that
print "not yet implemented". A VM per invocation, keyed by host PID to avoid collisions:

1. Copy-on-write overlay `/tmp/sandbox-session-<pid>.qcow2` on `cloud ← base ← layer ← session`.
   Only the base is created with an explicit `SANDBOX_DISK_SIZE` (20G); everything above inherits it.
2. One `-fsdev`/`virtio-9p-pci` pair per workspace directory (`share_args`), tagged by position
   (`projshare1`, `projshare2`, …) and mounted at `/workspace/<folder-name>`. Three more map `~/.cache/geli-sandbox/{npm,pip,apk}` so downloads
   persist across runs.
3. A cloud-init `user-data` + `meta-data` ISO as a second drive. For a session this only performs
   the mounts and writes `/etc/geli/session`; the baked-in `.bash_profile` waits on
   `cloud-init status --wait`, sources it, syncs and forces power off.
4. `-m 2G` and either `-enable-kvm -cpu host` or, where KVM is unavailable, `-cpu max` — the two
   move together, because `-cpu host` is KVM-only. **KVM is detected by opening
   `/dev/kvm`, not by stat'ing it** — on CI runners the device exists but is unusable, and an
   existence check calls that host accelerated and then fails at launch.
5. The extracted kernel is booted directly (`-kernel`/`-initrd`), skipping SeaBIOS, iPXE and the
   bootloader. Guest console on `ttyS1` → `console.log`; `ttyS0` is `mon:stdio` with inherited
   stdio, so the agent is fully interactive and sees nothing but its own output.
6. `SessionScratch` deletes the overlay and the share dir on `Drop`, honouring `GELI_KEEP`.

→ [`docs/why/images-and-boot.md`](docs/why/images-and-boot.md)

**`Drop` does not run on `process::exit`, so nothing between creating the scratch files and the end
of the session may exit the process.** `main` is a thin wrapper that prints the error and exits;
everything under it returns `Result`. → [`docs/why/workspaces-and-session.md`](docs/why/workspaces-and-session.md)

**Nor does `Drop` run on a signal, so every invocation starts by sweeping.** `sweep_dead_sessions`
deletes leftovers whose pid belongs to no running process **and whose owner is this user** — only
those, because several sessions share the machine and a live one must survive it; `hidepid` makes
another user's pid read as dead, so ownership is checked rather than left to a failing unlink.
**Everything per-session lives under one checked root** (`staging_root`, `/tmp/geli-<uid>`, mode
`0700`): `/tmp`'s sticky bit stops another user deleting geli's files, not creating a predictable
name before geli does. It stays on `/tmp` rather than `$XDG_RUNTIME_DIR` because the overlay grows
and that is the smaller filesystem. The sweep comes before a signal
handler on purpose: it also covers `SIGKILL`, which no handler can, and costs no dependency.

**A session's writes to `HOST_EXECUTED_PATHS` are named when it ends** — `.git/hooks`,
`.git/config`, `.envrc`, `.vscode/tasks.json` run on the *host*, outside any sandbox. The
comparison and the line come before `offer_to_persist_packages`, and nothing geli writes itself may
join the watched set. → [`docs/why/workspaces-and-session.md`](docs/why/workspaces-and-session.md)

**`agent_args` is shell-quoted per argument, and `GuestCommand` keeps the quoted `line` apart from
the raw `program`.** The matching helpers take the program; a quoted `'claude'` matches no recipe,
and the session would run with no credentials and say nothing useful.
→ [`docs/why/workspaces-and-session.md`](docs/why/workspaces-and-session.md)

### Output

**geli's own status goes to stderr, the command owns stdout** — including the interactive prompts.
`geli claude > out.txt` must capture the command and nothing else. `geli --list` and
`--build-image` are the two deliberate exceptions.

**Never advance the loading line on a timer.** It names the phase the guest actually reported
(`BOOT_PHASES`), and the readiness poll must keep its timeout. A guest that powers off early ends
the poll via `try_wait`, and the log is read once more after the child exits.
→ [`docs/why/output.md`](docs/why/output.md)

### Agents

**A recipe is a file in `agents/`, not code** — this is the contribution surface. `build.rs`
discovers them; adding an agent is one file and no Rust. `agent_for_command` matches the invoked
program, however it is pathed, and *only that agent's* credentials travel into the guest.

**`credentials` is a privileged field and recipes come from strangers.** `sensitive_credentials`
flags paths that are plainly not agent credentials, and the status block names every file that
leaves the user's home. **Credentials only, never the directory they sit in** — a test asserts no
recipe reaches into conversation history.
→ [`docs/why/agents-and-credentials.md`](docs/why/agents-and-credentials.md)

**Seeded config is not a credential, and the two must not be mixed.** `seed_files` synthesises
settings and answered onboarding from the mounted workspace and **nothing else** — no seed may read
the host's copy, and two tests enforce it. Seeding is *not* gated on `--no-credentials`.
→ [`docs/why/agents-and-credentials.md`](docs/why/agents-and-credentials.md)

**`seed_files` is the one place per-agent Rust is justified, so it is one `match` rather than
scattered.** Everything else about an agent stays data in `agents/*.toml`; an
`if agent.command == "agy"` out in `execute_sandbox` is how this went wrong once already.

**Never export an empty `ANTHROPIC_API_KEY`** — its presence outranks a claude.ai login, so a blank
export silently sends the session to API billing. `build_env_exports` filters empty values.
→ [`docs/why/agents-and-credentials.md`](docs/why/agents-and-credentials.md)

**Only `~/.claude/.credentials.json` is copied, never the rest of `~/.claude`**, and
`build_claude_config` seeds exactly three keys — `claude_config_holds_only_what_suppresses_the_prompts`
fails if the set grows. → [`docs/why/agents-and-credentials.md`](docs/why/agents-and-credentials.md)

### SSH (`--ssh`, `--ssh-key`, `--ssh-copy-key`)

Off by default. **`--ssh` forwards the host's `ssh-agent`; the private key never crosses.** geli
runs a relay thread, the guest reaches it at `10.0.2.2:<port>` through `socat` on
`/tmp/geli-agent.sock`, and `SSH_AUTH_SOCK` points there. `known_hosts` and a generated
`~/.ssh/config` still cross — those are about the hosts being talked to, not about the user.

**The relay is a filter, not a pipe.** Only `REQUEST_IDENTITIES` and `SIGN_REQUEST` reach the
host's agent; adding, removing and locking are refused, and `--ssh-key` rewrites the identities
answer so only that key is offered. **Refusing and reporting are separate**: an extension request
is denied silently because `ssh` negotiates one on every connection, and a warning that fires when
nothing is wrong hides the one that matters.

**Both of geli's loopback listeners ask for the session's token, and the check is one piece of
code** — `authorised` and `session_token` live in `net.rs`, because a CONNECT is network business,
and the relay imports them rather than the other way round. **A TCP port has no permissions**,
while the host's own agent socket sits behind a `0700` directory: without this, any local user
could sign as you and use the proxy as an open proxy to every allowed destination, with their
traffic landing in the closing `net:` line as if the session had made it.

Both carry it as HTTP Basic on a `CONNECT`, for different reasons that land in the same place: the
proxy because every client honouring `HTTPS_PROXY` already parses `http://geli:TOKEN@…`, and the
relay because **`socat` can send a `CONNECT` and cannot prepend bytes to a stream** — a shell
dialer that writes the token and then `cat`s was measured to deadlock `ssh-add` in two different
ways. It keeps other users of the host out, not the guest, which is the intended client.
→ [`docs/plans/10-paquetes-token-y-lo-que-no-se-puede-comprobar.md`](docs/plans/10-paquetes-token-y-lo-que-no-se-puede-comprobar.md)

**A connection that says who it is talking to is held to it.** `session-bind@openssh.com` is
accepted and its host signature *verified*; the method OpenSSH really uses is
`publickey-hostbound-v00@openssh.com`, not `publickey`, and it carries the destination inside the
signed data, so that is preferred over the binding and a disagreement between the two is refused.
**Verifying proves possession, not identity** — a guest can mint a host key and bind with it; what
it cannot do is claim a key it does not hold, and host keys are public. So **the report cannot
hide a signature, but a signature can go unnamed, and that is counted**: plain `publickey` is
mandatory in the protocol and binding is not, so a client that chooses neither leaves nothing to
name. **A key type geli cannot verify is declined in silence, never refused out loud** — a host
key signed by a CA is the ordinary case, and it is counted apart from a client that bound nothing.
**A signature that is not an authentication is never tied to a connection** — it has none, and it
is the only useful thing an agent does under `--restrict-net`. **None of this is policy**: where a
session signed is reported when it ends, never blocked — and when it does become policy, an
unbound authentication is refused by default, because the client in this image always binds.
→ [`docs/plans/09-atar-la-firma-a-su-conexion.md`](docs/plans/09-atar-la-firma-a-su-conexion.md)

**`--ssh-copy-key` is the old behaviour under a name that says what it does**, and it warns every
time. A copied key is readable inside the VM and outlives it.

**It is still not safe, and the wording must not claim otherwise**: while the session runs the
guest can ask for a signature over anything it can reach — **a network identity has no mount set**,
so the session reaches repositories it was never given. Permanent exposure becomes exposure bounded
by the session. The notice that says so fires when `--restrict-net` is *absent*, because that is
the one lever that closes it and the combination warning only ever named it the other way round. `.ssh` stays in `NEVER_COPY` so a recipe can never ask; the standing grant
lives in the registry, never in the project; host-key checking is strict when `known_hosts` came
across and `accept-new` only when it did not; and **`--restrict-net` still blocks SSH outright** —
the agent remains useful there for local signing, and gets its own nft rule rather than a widened
one. → [`docs/why/agents-and-credentials.md`](docs/why/agents-and-credentials.md)

### Egress policy (`--restrict-net`)

Off by default; with the flag the guest reaches only `DEFAULT_ALLOWED_HOSTS` plus the **workspace's**
`allow` list — never the project's `.geli.json`. A CONNECT proxy runs on a thread inside geli,
resolves on the host, and only dials port 443.

**Do not reach for `restrict=on` + `guestfwd`** — a guestfwd forwards exactly one connection and
then times out forever. **The route deletion is not the whole policy; the ruleset is**
(`src/guest/egress.nft`, default-drop, `inet` so IPv6 is covered), and **a ruleset that fails to
apply powers the session off**. **The lockdown's order is load-bearing**: route, then ruleset, then
sudo — the agent is root until the last step.
→ [`docs/why/egress.md`](docs/why/egress.md)

What this does **not** give: an allowed destination is still a way out, and anything ignoring the
proxy variables has no network rather than a direct path. That is the intended shape.

### Things to know when editing

- **The command goes in `.bash_profile`, never `.bashrc`** — `.bashrc` runs for every shell, so a
  subshell would re-run the command and `poweroff` mid-session.
- **The guest's `sandbox` user is created in the base setup script, not by cloud-init**, and its uid
  must match the host's or the agent can read the project but not write to it.
  → [`docs/why/guest-config.md`](docs/why/guest-config.md)
- **`write_files` runs before `runcmd`**, so base-recipe files are staged under `/etc/geli/` and
  installed into `/home/sandbox/` after the user exists.
- **There is no getty on `ttyS0`** — busybox `getty -n` writes an unconditional CRLF. The login
  helper runs straight from `inittab`. → [`docs/why/guest-config.md`](docs/why/guest-config.md)
- **`layer.sh` must remove `/tmp/.geli-session-active`**, or every session built on that layer takes
  the nested-shell branch and the command never runs. A test asserts it.
- **The serial console dictates `TERM` and 80x24**; `build_terminal_setup` overrides both from the
  host at launch. Serial lines carry no SIGWINCH, so a resize will not follow.
- **Sessions do not use the bootloader**, and `boot_cmdline` rewrites the image's own command line
  rather than inventing one. A kernel updated *inside* a session has no effect.
  → [`docs/why/images-and-boot.md`](docs/why/images-and-boot.md)
- **Every network step in a build is wrapped in `retry`** (`src/guest/retry.sh`, shared by the base
  recipe and every layer so the two cannot drift).
- **Every build is verified by a guarded sentinel** — cloud-init does not abort `runcmd` on failure,
  so `BUILD_OK_MARKER` sits behind a real check and `.building` is renamed only on success.
- **Deleting files in the guest does not shrink the qcow2**; the build drive runs `discard=unmap`
  and the recipe ends in `fstrim`. → [`docs/why/images-and-boot.md`](docs/why/images-and-boot.md)
- **Layers are siblings on the base, never stacked**, and **rebuilding the base deletes every
  layer** (`discard_layers`). One console log per image, not one per run.
- **Several sessions can run at once**; everything per-session is pid-keyed and `try_build_lock` is
  an `O_EXCL` file per image, holding its own pid so a killed build does not block every later one.
  → [`docs/why/images-and-boot.md`](docs/why/images-and-boot.md)
- **The memory ceiling is not a reservation** — peak RSS tracks the ceiling, not the workload. 2 GiB
  is the default; `GELI_MEMORY` overrides it. → [`docs/why/images-and-boot.md`](docs/why/images-and-boot.md)
- **Several agents in one VM** and **reusing a running VM** are rejected, not pending —
  `docs/plans/05-varios-agentes-por-vm.md` and `docs/plans/04-reutilizar-vm.md`.
- **The session script echoes a line before running the user's command**, so a silent command is
  distinguishable from a sandbox that never ran it.
- `agent_args` is captured with `trailing_var_arg` + `allow_hyphen_values`.
