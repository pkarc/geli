# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`geli` is a Rust CLI that runs an AI coding agent inside a disposable QEMU virtual machine. Isolation is at the hardware/VM level, not containers or namespaces — the agent gets a fresh Ubuntu cloud image each invocation, with only the registered project directories mounted in over 9p.

Usage: `geli <command> [args...]` runs `<command>` inside the sandbox; `geli --list` prints the workspace registry.

## Build / run

```bash
cargo build                    # debug build
cargo build --release          # release build
cargo test                     # unit tests (cloud-init generation)
cargo test cloud_init_parses   # single test
cargo run -- --list            # run without installing
./setup.sh                     # install host deps + Ubuntu image + install binary to /usr/local/bin
```

`GELI_KEEP=1 geli <cmd>` preserves the session qcow2 and `/tmp/sandbox-share-<pid>/` instead of
deleting them on exit — the only way to inspect `user-data` or the guest's
`/var/log/cloud-init-output.log` after a failed boot.

There is no CI in this repo yet.

Running the sandbox requires, on the host: `qemu-system-x86_64`, `qemu-img`, `genisoimage`, KVM access, and the base image at `~/qemu-sandbox/ubuntu-24.04-server-cloudimg-amd64.img`. `setup.sh` provisions all of these for Ubuntu hosts. `main.rs` re-checks qemu/genisoimage at runtime and exits with install instructions if missing.

## Architecture

Everything lives in `src/main.rs`, split into two layers:

**Workspace registry (platform-independent).** A "workspace" is a named group of directories that should see each other inside one VM. Two pieces of state:

- `./.geli.json` in each project dir — `{"workspace": "<name>"}`, binds that directory to a workspace. Absent on first run, so `get_or_create_workspace` prompts interactively (pick an existing workspace or name a new one) and writes it.
- `~/.config/geli/workspaces/<name>.txt` — one absolute path per line, the directories in that workspace. Appended to by `register_directory_to_workspace`; stale paths are filtered out at read time rather than pruned.

So invoking `geli` from directory A in workspace `foo` mounts *every* directory registered under `foo`, with A as the active one the command `cd`s into.

**Guest configuration (platform-independent, pure, tested).** `build_mount_script`, `build_cloud_init`, `build_env_exports` and `indent_block` turn a workspace into a cloud-init document with no I/O. Keeping these pure is what makes the guest config testable without booting a VM — add tests here rather than debugging through the serial console.

**Sandbox driver (`execute_sandbox`), `#[cfg]`-gated per OS.** Only the Linux implementation exists; macOS and Windows are stubs that print "not yet implemented".

The Linux path builds a VM per invocation, keyed by host PID to avoid collisions:

1. Copy-on-write overlay `/tmp/sandbox-session-<pid>.qcow2` backed by the read-only base image — the base is never modified. It is created at `SANDBOX_DISK_SIZE` (20G), not the base image's 3.5 GiB, because `apt install nodejs npm` alone overflows 3.5 GiB; cloud-init's growpart expands the root partition to match.
2. One `-fsdev`/`virtio-9p-pci` pair per workspace directory (tags `projshare1`, `projshare2`, …), mounted to `/workspace/<folder-name>` in the guest. Two extra 9p shares map `~/.cache/geli-sandbox/{npm,pip}` into the guest so package downloads persist across runs.
3. A cloud-init `user-data` + `meta-data` pair packed into an ISO by `genisoimage` and attached as a second drive. This is where the guest is actually programmed: perform the 9p mounts, install node/npm/python and `@anthropic-ai/claude-code`, enable autologin on `ttyS0`, then `cd` to the active folder, run the user's command and `sudo poweroff`.
4. QEMU is spawned with `-nographic -serial mon:stdio` and inherited stdio, so the agent is fully interactive in the host terminal.
5. On exit the overlay and `/tmp/sandbox-share-<pid>` are deleted — the session leaves nothing behind.

### Things to know when editing

- **The cloud-init YAML has a deliberately fixed shape.** Everything variable (mounts, env, the user's command) is injected as a literal block scalar via `indent_block`, so the document's structure never depends on the number of workspace directories. An earlier version built `runcmd` entries by string-replacing newlines; the indentation didn't match and the YAML never parsed, which fails *silently* — the VM boots fine and the command simply never runs. If you add anything variable here, put it in a `content: |` block, not in a list item, and extend `cloud_init_parses`.
- **The command goes in `.bash_profile`, never `.bashrc`.** `.bashrc` runs for every shell, so a subshell spawned by the agent would re-run the command and `poweroff` mid-session. The autologin getty gives a login shell, which is what reads `.bash_profile`.
- **`write_files` runs before `users-groups`** in cloud-init, so guest files are staged under `/etc/geli/` and copied into `/home/sandbox/` from `runcmd`, which runs after the user exists. Writing straight to the home directory would create it before `useradd` and break skel.
- `agent_args` is captured with `trailing_var_arg` + `allow_hyphen_values` and re-joined with spaces before going into the guest shell, so arguments are not shell-quoted.
- Workspace names are sanitized to lowercase alphanumerics plus `-`, because they become filenames and the guest hostname.
