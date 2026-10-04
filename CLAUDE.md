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
cargo test golden_cloud_init   # single test
cargo run -- --list            # run without installing
cargo run -- --build-image     # (re)build the golden image, ~5 min
./setup.sh                     # host deps + base image + binary + golden image
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

**Guest configuration (platform-independent, pure, tested).** `build_mount_script`, `build_cloud_init`, `build_golden_cloud_init`, `build_env_exports` and `indent_block` turn a workspace into a cloud-init document with no I/O. Keeping these pure is what makes the guest config testable without booting a VM — add tests here rather than debugging through the serial console.

**There are two cloud-init documents, and the split is the whole performance story.** `build_golden_cloud_init` is baked once by `--build-image` and holds everything static: packages, the agent, the autologin drop-in, and `GOLDEN_PROFILE` (the login profile). `build_cloud_init` runs per session and holds only what depends on the workspace: mounts, env, and the command. Anything slow or workspace-independent belongs in the golden recipe — putting it in the session document is what made boots take 4.5 minutes instead of 15 seconds.

**Sandbox driver (`execute_sandbox`), `#[cfg]`-gated per OS.** Only the Linux implementation exists; macOS and Windows are stubs that print "not yet implemented".

The Linux path builds a VM per invocation, keyed by host PID to avoid collisions:

1. Copy-on-write overlay `/tmp/sandbox-session-<pid>.qcow2` backed by the golden image, which is itself an overlay on the pristine base (`base ← golden ← session`). Only the golden image is created with an explicit `SANDBOX_DISK_SIZE` (20G) — the base's 3.5 GiB is overflowed by `apt install nodejs npm` alone — and sessions inherit that size from their backing file.
2. One `-fsdev`/`virtio-9p-pci` pair per workspace directory (tags `projshare1`, `projshare2`, …), mounted to `/workspace/<folder-name>` in the guest. Two extra 9p shares map `~/.cache/geli-sandbox/{npm,pip}` into the guest so package downloads persist across runs.
3. A cloud-init `user-data` + `meta-data` pair packed into an ISO by `genisoimage` and attached as a second drive. For a session this only performs the 9p mounts and writes `/etc/geli/session` (the `cd` plus the user's command); the baked-in `.bash_profile` waits on `cloud-init status --wait`, sources it, and powers off.
4. QEMU is spawned with `-nographic -serial mon:stdio` and inherited stdio, so the agent is fully interactive in the host terminal.
5. On exit the overlay and `/tmp/sandbox-share-<pid>` are deleted — the session leaves nothing behind.

### Things to know when editing

- **The cloud-init YAML has a deliberately fixed shape.** Everything variable (mounts, env, the user's command) is injected as a literal block scalar via `indent_block`, so the document's structure never depends on the number of workspace directories. An earlier version built `runcmd` entries by string-replacing newlines; the indentation didn't match and the YAML never parsed, which fails *silently* — the VM boots fine and the command simply never runs. If you add anything variable here, put it in a `content: |` block, not in a list item, and extend `cloud_init_parses`.
- **The command goes in `.bash_profile`, never `.bashrc`.** `.bashrc` runs for every shell, so a subshell spawned by the agent would re-run the command and `poweroff` mid-session. The autologin getty gives a login shell, which is what reads `.bash_profile`.
- **`write_files` runs before `users-groups`** in cloud-init. In the *golden* recipe the `sandbox` user does not exist yet, so files are staged under `/etc/geli/` and installed into `/home/sandbox/` from `runcmd`. In the *session* document the user already exists in the image, so `owner: sandbox:sandbox` works directly.
- **Autologin is baked into the image, so it races the session's cloud-init.** The getty can hand out a shell before `/etc/geli/session` has been written. `GOLDEN_PROFILE` blocks on `cloud-init status --wait` for exactly this reason; if the session file is still missing afterwards it drops to a shell with a pointer to the cloud-init log instead of powering off blind.
- **The guest has no Claude credentials and that is intentional.** Only `ANTHROPIC_API_KEY`/`OPENAI_API_KEY` are forwarded; the host's `~/.claude` OAuth credentials are not. With neither, `claude` sits in its first-run login flow forever, which is why `credential_warning` runs on the host *before* the VM boots. If you ever add credential forwarding, it is a security decision, not a convenience one.
- **The serial getty dictates `TERM=vt220` and a fixed 80x24.** `build_terminal_setup` overrides both from the host's terminal in `/etc/geli/env`, which `.bash_profile` sources after login — otherwise agent TUIs render in eight colours in a cramped window. Serial lines carry no SIGWINCH, so the size is a snapshot taken at launch and will not follow a resize.
- **The session script echoes a line before running the user's command.** A command that produces no output would otherwise be indistinguishable from a sandbox that never ran it — the exact failure mode that made the credential bug hard to diagnose.
- **A golden build is verified by a guarded sentinel.** cloud-init does *not* abort `runcmd` on failure, so the build VM only echoes `GOLDEN_OK_MARKER` behind `command -v claude && command -v git && command -v node`. The host greps the console log for it and refuses to publish the image otherwise — the `.building` file is renamed into place only on success.
- `agent_args` is captured with `trailing_var_arg` + `allow_hyphen_values` and re-joined with spaces before going into the guest shell, so arguments are not shell-quoted.
- Workspace names are sanitized to lowercase alphanumerics plus `-`, because they become filenames and the guest hostname.
