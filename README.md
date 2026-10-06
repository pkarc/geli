# geli

Run an AI coding agent inside a disposable virtual machine.

```bash
geli claude        # Claude Code
geli opencode      # OpenCode
geli agy           # Antigravity CLI
```

That boots a fresh Alpine VM, mounts your project into it, runs the agent interactively on the
serial console, and destroys the machine when you exit. The agent gets a real filesystem and a
real root shell — just not yours.

## Why a VM

Most agent sandboxes are containers. A container shares your kernel, so a container escape is a
kernel bug away. A VM escape is a much harder problem, which matters when the thing inside the
box is a program that writes and executes its own code.

The cost is boot time and memory. geli trades those for an isolation boundary you can reason
about.

## Status

Early. It works, with caveats worth knowing before you rely on it:

- **Linux only.** macOS and Windows have stubs that print "not implemented".
- **KVM is required**, not optional — `-enable-kvm` is hardcoded, so this will not run on a host
  without hardware virtualization exposed.
- **Network is open by default.** `--restrict-net` narrows it to an allowlist; without it the
  guest reaches anything. See [Restricting what the sandbox can reach](#restricting-what-the-sandbox-can-reach).
- **Agent versions are frozen into the image.** Rebuild with `geli --build-image` to update them.
- **The invoked agent's credential is copied into the sandbox** by default so it can work — see
  [Authentication](#authentication) for the trade and how to opt out.

## Requirements

- A Linux host with KVM (`/dev/kvm` accessible)
- `qemu-system-x86_64`, `qemu-img`, `genisoimage`
- Rust toolchain, to build
- ~315 MB of disk for the cloud and base images, plus ~200-380 MB per agent you run, plus a
  sparse 20 GB session overlay

## Install

```bash
git clone git@github.com:pkarc/geli.git
cd geli
./setup.sh
```

`setup.sh` installs the host packages, downloads the Alpine cloud image into
`~/qemu-sandbox/`, builds the release binary, copies it to `/usr/local/bin/`, and provisions the
base image. It uses `sudo` for the package install and the final copy.

That last step boots a VM once to install node, python, git and bash — two or three minutes. No
agent is installed there: the first time you run one, geli builds its layer, says so, and takes
a minute or two. Every run after that starts in seconds.

## Usage

```bash
geli <command> [args...]    # run a command inside the sandbox
geli --restrict-net <cmd>   # let the sandbox reach only an allowlist of hosts
geli --list                 # show the workspace registry
geli --build-image          # build the base image if it is missing or stale
geli --build-image --agents claude,opencode   # and those agents' layers, now instead of on first use
geli --no-credentials <cmd> # do not copy any credentials into the sandbox
```

A session starts in **about nine seconds**, because it installs nothing and boots the image's
kernel directly — see [The images](#the-images).

The first run in a directory asks how to namespace it and writes a `.geli.json`:

```
$ geli claude
[-] No local sandbox configuration found for this directory.
[?] How would you like to namespace this folder?
------------------------------------------------
  1) Add to existing workspace: [clientwork]
  n) Create a BRAND NEW workspace
------------------------------------------------
Select an option (1-1 or n):
```

### Workspaces

A workspace is a named group of directories that should see each other inside the same VM. If
your backend and frontend are separate repos but one task spans both, put them in one workspace
and the agent can work across them:

```bash
cd ~/code/api       && geli claude   # workspace: acme
cd ~/code/dashboard && geli claude   # same workspace
```

Both runs mount both repos at `/workspace/api` and `/workspace/dashboard`. The directory you
launched from is the one the agent starts in.

Two pieces of state back this:

| Path | Role |
|---|---|
| `./.geli.json` | Binds this directory to a workspace. Local to your machine; gitignored. |
| `~/.config/geli/workspaces/<name>.txt` | The directories in that workspace, one path per line. |

There is no command to remove a directory from a workspace yet — edit the `.txt` file. Paths that
no longer exist are skipped automatically.

## The images

Installing the toolchain and the agents on every boot cost about four and a half minutes per
run. Instead it happens once, into reusable images — and the agents are kept out of the shared
one, so you only pay for the ones you actually run:

| File in `~/qemu-sandbox/` | Role |
|---|---|
| `nocloud_alpine-3.22.2-...qcow2` | Pristine Alpine cloud image. Never written to. |
| `geli-base.qcow2` | Overlay on it with the toolchain, the sandbox user and autologin. **No agent.** |
| `geli-layer-<agents>.qcow2` | One agent, installed on top of the base image. |
| `geli-base.recipe`, `geli-layer-<agents>.recipe` | Hash of the recipe each was built from. |
| `geli-vmlinuz`, `geli-initramfs` | Kernel and initramfs handed out by the base build. Sessions boot these directly, skipping firmware and bootloader. |
| `geli-base.meta`, `geli-layer-<agents>.meta` | The kernel command line, and the version each image added. |

So the chain is **cloud image ← base ← agent layer ← session**, and a session is a throwaway
overlay on top. QEMU boots `geli-vmlinuz` directly rather than going through firmware and a
bootloader, so a session is a kernel boot plus mounting your directories: **about nine seconds**,
against the four and a half minutes it started at.

`geli --build-image` builds the base image only. An agent's layer is built the first time you run
that agent — once, with a line saying so — or ahead of time:

```bash
geli --build-image --agents claude,opencode
```

Rebuilding the base image discards every layer, because a layer's backing file is exactly the
image that was just replaced. They are rebuilt on next use.

What it costs, measured with `du` on the real images:

| | |
|---|---|
| Alpine cloud image | 185 MB |
| `geli-base.qcow2` | 130 MB |
| `geli-layer-claude.qcow2` | 245 MB |
| `geli-layer-opencode.qcow2` | 206 MB |
| `geli-layer-agy.qcow2` | 377 MB |

So `geli claude` on a fresh install costs 560 MB all in, against 1,107 MB for the single image
that held all three agents. **The saving is in the common case, not the total**: all three layers
together come to 958 MB, about 4% *more* than the 922 MB one-image build, because each layer pays
its own qcow2 and npm overhead. What changes is that you no longer pay for OpenCode and
Antigravity to run Claude Code — and that a tenth agent costs the people who do not use it
nothing.

There is a second, less obvious gain. A recipe that breaks now breaks one layer. While writing
this, Antigravity's installer started serving its shell script gzipped, which made `agy`'s recipe
download a binary and fail; `claude` and `opencode` were unaffected and still built. Under the
single image that same failure took the whole build down with it.

If the recipe in the binary no longer matches `geli-base.recipe`, geli warns and keeps going — a
stale image is out of date, not broken. A layer whose recipe changed is simply rebuilt, and a
layer's recorded hash includes its parent's, so editing one agent's TOML invalidates that agent
and leaves the others alone.

Moving `~/qemu-sandbox/` breaks the chain: qcow2 records its backing file by absolute path.
Rebuild rather than trying to repair it.

## How it works

Each invocation:

1. Creates a copy-on-write qcow2 overlay on top of the invoked agent's layer, building that layer
   first if this is the first run of that agent. Nothing below the session is written to. A
   command that is not an agent overlays the base image directly, which is the cheapest boot
   geli has.
2. Attaches each workspace directory as a virtio-9p share, mounted at `/workspace/<folder>`.
3. Generates a cloud-init ISO that performs the mounts and drops in your command. Autologin and
   the login profile already live in the image.
   The guest's boot console goes to a log file, not your terminal, so a session prints geli's own
   lines and your command's output — nothing else. geli's status goes to stderr, so stdout is
   yours alone.
4. Launches QEMU with 4 GB RAM, 2 vCPUs and inherited stdio, so the agent is fully interactive in
   your terminal.
5. Deletes the overlay and temporary files on exit.

`~/.cache/geli-sandbox/{npm,pip}` is mounted into the guest so package downloads survive across
sessions.

### Environment

| Variable | Effect |
|---|---|
| `ANTHROPIC_API_KEY` | Forwarded into the guest. |
| `OPENAI_API_KEY` | Forwarded into the guest. |
| `GELI_KEEP=1` | Keep the session disk and cloud-init files on exit, for debugging. |
| `TERM`, `COLORTERM` | Forwarded, so the agent's TUI gets your colours instead of the serial console's `vt220`. |

Your terminal's size is forwarded too, at launch. Serial consoles carry no `SIGWINCH`, so resizing
the window mid-session will not reach the guest — restart the session to pick up a new size.

## Agents

geli knows three terminal agents — Claude Code, OpenCode and Antigravity CLI — and runs whatever
command you give it, so anything else in the image works too.

Knowing the agent by name buys three things: it gets its own qcow2 layer, built on first use, so
the image you boot holds that agent and nothing else; **only its credentials are copied into the
guest**; and `--restrict-net` opens only the hosts it talks to. Running `geli opencode` puts no
Claude token in the sandbox — and no Claude Code in the image either.

### Teaching geli a new agent

One file in `agents/`, no Rust:

```toml
# agents/aider.toml
command     = "aider"
binary      = "aider"
label       = "aider credentials"
credentials = [".aider.conf.yml"]
hosts       = ["api.openai.com"]
install     = """
retry pip install --break-system-packages aider-chat"""
# optional; defaults to `<binary> --version`, trimmed to its first line
version     = "aider --version | cut -d' ' -f2"
```

`install` runs as root inside that agent's own qcow2 layer, with a `retry` helper in scope for
anything that touches the network. `binary` is checked before the layer is published, so a recipe
that silently fails to install cannot ship — and since each agent is its own layer, a broken
recipe costs nothing to anyone not running it.

`credentials` deserves care, both to write and to review: those files are copied out of the
user's home into a VM that can reach the network. Name the credential, never the directory it
sits in — agents tend to keep conversation history next to their tokens. geli refuses to be quiet
about it: every copied path is printed at the start of each session, and paths like `.ssh` or
`.aws` raise a warning.

## Authentication

The sandbox is a fresh machine with no Claude state, so credentials have to come from the host.
By default geli copies `~/.claude/.credentials.json` in, so `geli claude` just works and bills
your Claude plan:

```bash
geli claude
```

That one file is all geli copies. Your account profile, conversation transcripts, prompt history
and file snapshots across every project you have worked on stay on the host.

The sandbox is disposable, so the agent would otherwise re-run its first-run prompts every
session — approve the key, pick a theme, trust the folder. geli writes a small config into the
guest that answers exactly those three, and nothing more.

Use an API key instead by exporting one; it takes precedence over the copied credentials, and
bills API credits rather than your plan:

```bash
export ANTHROPIC_API_KEY=sk-ant-...   # scope it to geli, see below
geli claude
```

`geli --no-credentials claude` forwards neither. With no credentials at all, `claude` starts its
first-run login flow inside the VM and waits for input that never arrives, so the session looks
like it has hung — geli checks before booting and warns you instead.

> **Careful with a global `export ANTHROPIC_API_KEY`.** It also takes precedence over your *host*
> Claude Code login, silently switching your everyday `claude` from your plan to API billing. To
> scope it to geli alone, wrap it in your shell rc instead:
>
> ```zsh
> geli() { ANTHROPIC_API_KEY="sk-ant-..." command geli "$@"; }
> ```

### What copying the credential means

The agent inside the sandbox can read your Claude account token, and the guest network is
currently unrestricted. That is a deliberate trade: geli's job is to keep the agent away from
files you did not give it, and a credential is not one of those. If you would rather not make it,
`--no-credentials` plus a scoped API key gives you a revocable credential instead.

One caveat: the copy is one-way. If the token is refreshed inside the sandbox, the new one dies
with the VM. Should a refresh ever invalidate the host's copy, re-run `claude` on the host to log
back in.

## Restricting what the sandbox can reach

By default the guest reaches the whole internet. `--restrict-net` narrows it to an allowlist:
Anthropic's endpoints plus the registries an agent needs to work — npm, PyPI, GitHub, crates.io.
A project can add its own in `.geli.json`:

```json
{ "workspace": "acme", "allow": ["registry.internal.example"] }
```

A proxy inside geli resolves and filters on the host; the guest's default route is deleted so the
proxy is the only way out. Blocked attempts are logged and summarised when the session ends:

```
geli · net: 14 allowed, 9 blocked (http-intake.logs.us5.datadoghq.com, mcp-proxy.anthropic.com)
```

That summary is worth having on its own — it tells you what your agent reaches for.

**Restricted mode also takes away the agent's root** inside the guest, leaving it only
`sudo poweroff`. It has to: the agent is root by default and could simply restore the route. The
cost is that `apk add` no longer works mid-session — the image already carries node, python, git
and the agent, so project dependencies installed as the user are unaffected.

### What it does not give you

- **An allowed destination is still a way out.** With `github.com` reachable, an agent can push to
  a gist. The allowlist stops an *arbitrary* server from being reached; it does not make the data
  unable to leave.
- **DNS queries still resolve.** slirp's resolver sits on the same subnet as the proxy, so names
  remain a slow, noisy channel out. Closing it needs a packet filter in the image.
- **Anything that ignores proxy variables loses the network entirely**: `git` over SSH, `ping`,
  raw sockets, and busybox `wget` (which sends plaintext absolute-form requests a CONNECT proxy
  correctly refuses). `npm`, `pip` and `git` over HTTPS all work.

## Security notes

The VM boundary protects your host filesystem. It does not protect everything, and the gaps are
worth stating plainly:

- **The guest has unrestricted network access by default.** An agent that wants to exfiltrate the
  code it was given can do so. `--restrict-net` narrows that to an allowlist — see
  [Restricting what the sandbox can reach](#restricting-what-the-sandbox-can-reach) for what it
  does and does not buy you.
- **Your credentials are handed to the agent.** An API key, or your copied Claude account token,
  is written into the guest because the agent needs it. The sandbox protects your files, not your
  credential. Only `.credentials.json` is copied — never your Claude history or transcripts.
- **Only the directories in the workspace are visible.** Everything else on your machine is not
  reachable from inside.

## Development

```bash
cargo build
cargo test                    # every test runs on a pure function; none boots a VM
cargo test cloud_init_parses  # a single test
cargo clippy --all-targets
```

```
src/agents.rs   the agents geli knows, parsed from agents/*.toml
src/guest.rs    what the guest is told to be: cloud-init, recipes, mounts, boot
src/net.rs      egress policy: the allowlist and the proxy that enforces it
src/qemu.rs     images on disk, direct kernel boot, watching the guest come up
src/ui.rs       the status block and the loading line
agents/*.toml   one recipe per agent
src/guest/*.sh  the recipes themselves, as real shell and YAML
```

The guest's configuration is generated by pure functions so it can be tested without booting a
VM, and that is not a stylistic preference: a malformed cloud-init document fails **silently** —
the VM boots normally and your command simply never runs. If you change what the guest does, add
a test rather than debugging through the serial console.

Before and after a refactor, `cargo test dump_generated_documents -- --ignored` writes every
generated guest document to `/tmp/geli-baseline`; diffing the two is how the module split and the
move to file-based recipes were shown to change nothing.

### CI

| Workflow | When | What |
|---|---|---|
| `check` | every push and PR | build, test, clippy `-D warnings`. No VM. |
| `image` | PRs touching recipes or code, plus weekly | builds the base image and the affected layers, then runs a session per agent |

`check` proves a recipe parses and declares sane things. Only `image` proves it installs: cloud-init
does not abort on failure, so the honest answer comes from booting the VM and looking for the
binary. A PR that changes one recipe builds one layer, which is what makes this cheap enough to run
at all — and the weekly run is there because upstream breaks recipes with no commit here.

The session smoke test asserts that `geli <agent> --version` puts **exactly one line on stdout**.
That is the stdout-belongs-to-the-command contract, and it is the assertion that would have caught
the stray blank line busybox `getty -n` used to print at the top of every session.

`cargo fmt --check` is not a gate: parts of this code are laid out by hand in ways rustfmt would
undo. If you want it, reformat in a commit of its own.

If you send a recipe, note that `image` runs its `install` as root in a VM on a runner. The
workflow uses `pull_request`, never `pull_request_target`, so a fork's job has a read-only token
and no secrets.

[CLAUDE.md](CLAUDE.md) has the architecture and the hard-won details. [docs/plans/](docs/plans/)
holds the plans and measurements behind the bigger decisions — why Alpine, why not Debian, what
Antigravity's credential actually weighs.

## Roadmap

Done:

1. ~~Make the sandbox boot and run the command~~
2. ~~A prebuilt image, so a session installs nothing~~ — 4.5 min → 15 s
3. ~~Alpine as the guest base~~ — a third of Ubuntu's disk at the same speed
4. ~~Quiet output and direct kernel boot~~ — 615 lines of console → 1 on stdout; 14 s → 9 s
5. ~~Network egress policy~~ — `--restrict-net`
6. ~~More than one agent~~ — OpenCode and Antigravity alongside Claude Code
7. ~~Recipes as data~~ — one TOML file per agent, no Rust
8. ~~A qcow2 layer per agent~~ — the base image carries no agent; each one is a cached layer
9. ~~CI~~ — `check` on every push; `image` builds the affected layers and runs a real session

Next:

10. **Let one session carry several agents.** The layer chain already supports it —
    `geli-layer-claude+opencode.qcow2` is a layer on `geli-layer-claude.qcow2`, keyed by the
    sorted set — but nothing asks for more than the invoked agent yet. The open question is
    credentials: today only the invoked agent's travel, and an agent you asked to have in the VM
    but whose credential stays out is not obviously useful.
11. **Close the DNS channel.** `--restrict-net` leaves slirp's resolver reachable, so names are
    still a slow way out. Needs a packet filter in the image.
12. Optional KVM, for hosts without hardware virtualization.
13. Trim the 4 GB RAM ceiling — the guest uses 476 MB.
