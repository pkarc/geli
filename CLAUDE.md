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

### SSH keys (`--ssh`, `--ssh-key`)

Off by default. With the flag the host's `~/.ssh/id_*` — or one named key — is copied in, along with
`known_hosts` and a generated `~/.ssh/config`.

**This is a different order of risk from forwarding an agent credential.** A private key grants
write access to every repository the user has and outlives the VM that leaked it. `.ssh` is in
`NEVER_COPY` so a recipe can never ask for it; **the standing grant lives in the registry, never in
the project**; host-key checking is strict when `known_hosts` came across and `accept-new` only when
it did not; and **`--restrict-net` blocks SSH outright**, which the warning must say plainly.
→ [`docs/why/agents-and-credentials.md`](docs/why/agents-and-credentials.md)

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
