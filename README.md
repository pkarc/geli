# geli

Run an AI coding agent inside a disposable virtual machine.

```bash
geli claude        # Claude Code
geli opencode      # OpenCode
geli agy           # Antigravity CLI
```

That boots a fresh Alpine VM, mounts your project into it, runs the agent interactively on the
serial console, and destroys the machine when you exit. The agent gets a real root shell and a
real filesystem — just not yours.

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
- **Network is open.** The guest has unrestricted outbound access. This is a deliberate choice
  for now, not an oversight — see [Security notes](#security-notes).
- **The agent version is frozen into the image.** Rebuild with `geli --build-image` to update it.
- **Your Claude account token is copied into the sandbox** by default so the agent can work —
  see [Authentication](#authentication) for the trade and how to opt out.

## Requirements

- A Linux host with KVM (`/dev/kvm` accessible)
- `qemu-system-x86_64`, `qemu-img`, `genisoimage`
- Rust toolchain, to build
- ~3 GB of disk for the base and golden images, plus a sparse 20 GB session overlay

## Install

```bash
git clone git@github.com:pkarc/geli.git
cd geli
./setup.sh
```

`setup.sh` installs the host packages, downloads the Alpine cloud image into
`~/qemu-sandbox/`, builds the release binary, copies it to `/usr/local/bin/`, and provisions the
golden image. It uses `sudo` for the package install and the final copy.

That last step boots a VM once to install node, npm, python, git and the agent, and takes a few
minutes. It is what makes every subsequent session start in seconds.

## Usage

```bash
geli <command> [args...]    # run a command inside the sandbox
geli --restrict-net <cmd>   # let the sandbox reach only an allowlist of hosts
geli --list                 # show the workspace registry
geli --build-image          # rebuild the golden image (also how you update the agent)
geli --no-credentials <cmd>  # do not copy your Claude credentials into the sandbox
```

A session starts in **~15 seconds**, because it installs nothing — see
[The golden image](#the-golden-image).

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

## The golden image

Installing node, npm, python and the agent on every boot cost ~4.5 minutes per run. Instead that
happens once, into a reusable image:

| File in `~/qemu-sandbox/` | Role |
|---|---|
| `nocloud_alpine-3.22.2-...qcow2` | Pristine Alpine cloud image. Never written to. |
| `geli-golden.qcow2` | Overlay on the base with the toolchain, the agent and autologin baked in. |
| `geli-golden.recipe` | Hash of the recipe it was built from. |
| `geli-vmlinuz`, `geli-initramfs` | Kernel and initramfs handed out by the build. Sessions boot these directly, skipping firmware and bootloader. |
| `geli-golden.meta` | The kernel command line and the versions inside the image. |

Sessions are overlays on the golden image, so a session boot is just a kernel boot plus mounting
your directories: **~15 seconds instead of ~4.5 minutes.**

If the recipe in the binary no longer matches `geli-golden.recipe`, geli warns and keeps going —
a stale image is out of date, not broken. If the image is missing it stops and tells you to run
`geli --build-image`.

Moving `~/qemu-sandbox/` breaks the golden image: qcow2 records its backing file by absolute
path. Rebuild it rather than trying to repair the chain.

## How it works

Each invocation:

1. Creates a copy-on-write qcow2 overlay on top of the golden image. Neither the golden image nor
   the base is written to.
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

The image carries three terminal agents — Claude Code, OpenCode and Antigravity CLI — and geli
runs whatever command you give it, so anything else in the image works too.

Knowing the agent by name buys three things: it is installed for you, **only its credentials are
copied into the guest**, and `--restrict-net` opens only the hosts it talks to. Running
`geli opencode` puts no Claude token in the sandbox.

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
```

`install` runs as root while the image is built, with a `retry` helper in scope for anything that
touches the network. `binary` is checked before the image is published, so a recipe that silently
fails to install cannot ship.

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

- **The guest has unrestricted network access.** An agent that wants to exfiltrate the code it was
  given can do so. Restricting egress to an allowlist is planned; today the only reason it is open
  is that `apt` and `npm` run on every boot.
- **Your credentials are handed to the agent.** An API key, or your copied Claude account token,
  is written into the guest because the agent needs it. The sandbox protects your files, not your
  credential. Only `.credentials.json` is copied — never your Claude history or transcripts.
- **Only the directories in the workspace are visible.** Everything else on your machine is not
  reachable from inside.

## Development

```bash
cargo build
cargo test                    # cloud-init generation is unit tested
cargo test cloud_init_parses  # single test
cargo clippy --all-targets
```

The guest configuration is generated by pure functions (`build_cloud_init`, `build_mount_script`,
`indent_block`) specifically so it can be tested without booting a VM. If you change what the
guest does, add a test there rather than debugging through the serial console — a malformed
cloud-init document fails *silently*: the VM boots normally and your command simply never runs.

See [CLAUDE.md](CLAUDE.md) for architecture detail and [docs/plans/](docs/plans/) for the roadmap.

## Roadmap

1. ~~Make the sandbox boot and run the command~~ — done
2. ~~Golden image, to cut boot from minutes to seconds~~ — done (~4.5 min → ~15 s)
3. ~~Alpine as the guest base~~ — done (1.9 GB → 656 MB, same session time)
4. ~~Quiet output and direct kernel boot~~ — done (615 lines → 8; ~14 s → ~7.5 s)
5. ~~Network egress policy~~ — done (`--restrict-net`); DNS remains an open channel.
6. Optional KVM, for hosts without hardware virtualization.
7. Trim the 4 GB RAM ceiling — the guest uses 476 MB.
