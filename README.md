# geli

Run an AI coding agent inside a disposable virtual machine.

```bash
geli claude
```

That boots a fresh Ubuntu VM, mounts your project into it, runs the agent interactively on the
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
- **Boot takes ~4.5 minutes.** Every run installs node, npm, python and the agent from scratch.
  A prebuilt golden image is the next planned change.
- **Network is open.** The guest has unrestricted outbound access. This is a deliberate choice
  for now, not an oversight — see [Security notes](#security-notes).

## Requirements

- A Linux host with KVM (`/dev/kvm` accessible)
- `qemu-system-x86_64`, `qemu-img`, `genisoimage`
- Rust toolchain, to build
- ~20 GB of free disk for the session overlay (sparse; actual use is far lower)

## Install

```bash
git clone git@github.com:pkarc/geli.git
cd geli
./setup.sh
```

`setup.sh` installs the host packages, downloads the Ubuntu 24.04 cloud image into
`~/qemu-sandbox/`, builds the release binary and copies it to `/usr/local/bin/`. It uses `sudo`
for the package install and the final copy.

## Usage

```bash
geli <command> [args...]    # run a command inside the sandbox
geli --list                 # show the workspace registry
```

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

## How it works

Each invocation:

1. Creates a copy-on-write qcow2 overlay on top of the Ubuntu base image. The base is never
   written to.
2. Attaches each workspace directory as a virtio-9p share, mounted at `/workspace/<folder>`.
3. Generates a cloud-init ISO that installs the toolchain, performs the mounts, enables autologin
   on `ttyS0`, runs your command and powers off.
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

## Security notes

The VM boundary protects your host filesystem. It does not protect everything, and the gaps are
worth stating plainly:

- **The guest has unrestricted network access.** An agent that wants to exfiltrate the code it was
  given can do so. Restricting egress to an allowlist is planned; today the only reason it is open
  is that `apt` and `npm` run on every boot.
- **Your API keys are handed to the agent.** They are written into the guest environment, because
  the agent needs them. The sandbox does not protect the credential, only the host.
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
2. Golden image, to cut boot from minutes to seconds
3. Network egress policy
4. Optional KVM, for hosts without hardware virtualization
