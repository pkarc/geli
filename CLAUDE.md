# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`geli` is a Rust CLI that runs an AI coding agent inside a disposable QEMU virtual machine. Isolation is at the hardware/VM level, not containers or namespaces — the agent gets a fresh Alpine cloud image each invocation, with only the registered project directories mounted in over 9p.

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

Running the sandbox requires, on the host: `qemu-system-x86_64`, `qemu-img`, `genisoimage`, KVM access, and the base image at `~/qemu-sandbox/nocloud_alpine-3.22.2-x86_64-bios-cloudinit-r0.qcow2`. `setup.sh` provisions all of these for Ubuntu/Debian hosts. `GELI_BASE_IMAGE` points `--build-image` at a different cloud image. `main.rs` re-checks qemu/genisoimage at runtime and exits with install instructions if missing.

## Architecture

```
src/main.rs     CLI, the workspace registry, orchestration, tests
src/agents.rs   the agents geli knows how to host
src/guest.rs    what the guest is told to be: cloud-init, mounts, boot phases
src/net.rs      egress policy: allowlist, CONNECT parsing, proxy env
src/qemu.rs     the Linux driver: images, direct boot, proxy server, boot watch
src/guest/*.sh  the recipes themselves, as real shell and YAML
```

Recipes are files, not Rust strings, with `@NAME@` placeholders. They were literals until every
`{` in a shell script had to be doubled to survive `format!`. If you change one, the
`#[ignore]`d `dump_generated_documents` test writes every generated guest document to `/tmp` —
dump before and after and diff, which is how the split above was proven to change nothing.

Two layers underneath:

**Workspace registry (platform-independent).** A "workspace" is a named group of directories that should see each other inside one VM. Two pieces of state:

- `./.geli.json` in each project dir — `{"workspace": "<name>"}`, binds that directory to a workspace. Absent on first run, so `get_or_create_workspace` prompts interactively (pick an existing workspace or name a new one) and writes it.
- `~/.config/geli/workspaces/<name>.txt` — one absolute path per line, the directories in that workspace. Appended to by `register_directory_to_workspace`; stale paths are filtered out at read time rather than pruned.

So invoking `geli` from directory A in workspace `foo` mounts *every* directory registered under `foo`, with A as the active one the command `cd`s into.

**Guest configuration (platform-independent, pure, tested).** `build_mount_script`, `build_cloud_init`, `build_golden_cloud_init`, `build_env_exports` and `indent_block` turn a workspace into a cloud-init document with no I/O. Keeping these pure is what makes the guest config testable without booting a VM — add tests here rather than debugging through the serial console.

**There are two cloud-init documents, and the split is the whole performance story.** `build_golden_cloud_init` is baked once by `--build-image` and holds everything static: packages, the agent, the autologin drop-in, and `GOLDEN_PROFILE` (the login profile). `build_cloud_init` runs per session and holds only what depends on the workspace: mounts, env, and the command. Anything slow or workspace-independent belongs in the golden recipe — putting it in the session document is what made boots take 4.5 minutes instead of 15 seconds.

**Sandbox driver (`execute_sandbox`), `#[cfg]`-gated per OS.** Only the Linux implementation exists; macOS and Windows are stubs that print "not yet implemented".

The Linux path builds a VM per invocation, keyed by host PID to avoid collisions:

1. Copy-on-write overlay `/tmp/sandbox-session-<pid>.qcow2` backed by the golden image, which is itself an overlay on the pristine base (`base ← golden ← session`). Only the golden image is created with an explicit `SANDBOX_DISK_SIZE` (20G) — Alpine's base is 202 MiB virtual, which `apk add nodejs npm` alone overflows — and sessions inherit that size from their backing file.
2. One `-fsdev`/`virtio-9p-pci` pair per workspace directory (tags `projshare1`, `projshare2`, …), mounted to `/workspace/<folder-name>` in the guest. Two extra 9p shares map `~/.cache/geli-sandbox/{npm,pip}` into the guest so package downloads persist across runs.
3. A cloud-init `user-data` + `meta-data` pair packed into an ISO by `genisoimage` and attached as a second drive. For a session this only performs the 9p mounts and writes `/etc/geli/session` (the `cd` plus the user's command); the baked-in `.bash_profile` waits on `cloud-init status --wait`, sources it, syncs and forces power off.
4. QEMU boots the extracted kernel directly (`-kernel`/`-initrd`), skipping SeaBIOS, iPXE and the bootloader — ~6s of the old session time. The guest's console goes to `ttyS1`, written to `console.log` in the session's share dir; `ttyS0` is `mon:stdio` with inherited stdio, so the agent is fully interactive and sees nothing but its own output.
5. On exit the overlay and `/tmp/sandbox-share-<pid>` are deleted — the session leaves nothing behind.

### Output

geli's own status goes to **stderr**, the command owns **stdout**. `geli claude > out.txt` must capture the command and nothing else. Before launch it prints a block with what is mounted, which credential is in play, and what is inside the image — the facts that change per invocation; anything identical every session is noise even when geli writes it. A loading line runs until the guest signals readiness, then clears itself. It names the phase the guest is actually in — `starting`, `boot`, `network`, `cloud-init`, `mounts` — and every one of those is a string the guest writes to its own boot console (`BOOT_PHASES`). **Never advance it on a timer:** a progress indicator that moves on a clock is confidently wrong exactly when the boot is stuck, which is the only time anyone reads it.

Readiness is a marker `mounts.sh` echoes as its last act. `runcmd` output lands in the boot console log, which `track_boot` polls for both the phase and the signal. The poll **must** keep its timeout: a guest whose cloud-init broke will never answer, and the terminal has to be handed over anyway.

Colour is on only when stderr is a terminal and `NO_COLOR` is unset; piped, the same phases print one line each so a CI log still shows where a boot died.

### Agents

`AGENTS` is the table of terminal agents the sandbox knows: claude, opencode and agy
(Antigravity). Each declares how it installs, the binary that proves the build worked, the
credential paths it needs, and the hosts to open when egress is restricted. The golden image
carries all three; `agent_for_command` matches the first word of the user's command, however it
is pathed, and *only that agent's* credentials travel into the guest. An unrecognised command
(`geli bash`) carries none.

**Credentials only, never the directory they sit in.** Antigravity keeps 1.8 KB of OAuth token in
`~/.gemini/oauth_creds.json` — next to a 1.3 GB index and 317 MB of conversations in the same
tree. Claude's is the same shape. A test asserts no agent's credential list reaches into history.

**Node packages that ship per-platform binaries need pruning.** `opencode-ai` installs glibc,
musl and "baseline" variants at ~180 MB each and hardlinks the right one into `bin/`; deleting
the other three took it from 728 MB to 187 MB with the CLI still working.

**Deleting files in the guest does not shrink the qcow2.** Blocks written during a build stay
allocated. The build drive runs with `discard=unmap` and the recipe ends in `fstrim`, which took
the image from 1.9 GB to 922 MB. Anything that writes-then-deletes during provisioning depends
on this.

**Antigravity's `agy` is a statically linked Go binary**, so musl never enters into it — unlike
every npm-delivered agent, where it does.

### Egress policy (`--restrict-net`)

Off by default; with the flag the guest reaches only `DEFAULT_ALLOWED_HOSTS` plus whatever the project's `.geli.json` lists under `allow`. A CONNECT proxy runs on a thread inside geli, resolves on the host, and only dials port 443 — a proxy that reaches any port on an allowed host is a general tunnel, not a policy.

**Do not reach for `restrict=on` + `guestfwd`.** Measured on QEMU 8.2.2: a guestfwd forwards exactly one connection and then times out forever, with or without `restrict`, so a session died after its first request. The guest reaches the proxy at slirp's host alias `10.0.2.2:<port>` instead, and egress is cut by deleting the guest's default route — which leaves the internet unreachable by name *and* by IP while the on-link proxy still answers.

**Cutting the route is only half of it.** The agent is root in the guest by default and can add the route straight back, so restricted mode also narrows its sudoers to `/sbin/poweroff` alone. Both happen in `mounts.sh`, as root, before the agent runs — and before the readiness marker, or the agent could start first. A restriction the sandbox can undo is worse than none, because the status line claims it is on.

What this does **not** give: an allowed destination is still a way out — `github.com` permits a gist. And slirp's DNS at `10.0.2.3` stays on-link, so name queries remain a low-bandwidth channel; closing that needs nftables in the image.

### Things to know when editing

- **The cloud-init YAML has a deliberately fixed shape.** Everything variable (mounts, env, the user's command) is injected as a literal block scalar via `indent_block`, so the document's structure never depends on the number of workspace directories. An earlier version built `runcmd` entries by string-replacing newlines; the indentation didn't match and the YAML never parsed, which fails *silently* — the VM boots fine and the command simply never runs. If you add anything variable here, put it in a `content: |` block, not in a list item, and extend `cloud_init_parses`.
- **The command goes in `.bash_profile`, never `.bashrc`.** `.bashrc` runs for every shell, so a subshell spawned by the agent would re-run the command and `poweroff` mid-session. The autologin helper runs `login -f`, which gives a login shell, which is what reads `.bash_profile`.
- **The guest's `sandbox` user is created in the golden setup script, not by cloud-init.** Alpine's users module cannot set an explicit `uid` and fails the whole module when asked to — silently, leaving no user, which then breaks `write_files` entries that specify an owner, which leaves `/etc/geli/mounts.sh` unwritten, which leaves the workspace unmounted. The uid must match the host's: files arrive over 9p owned by the host user, so a mismatch leaves the agent able to read the project but not write to it. Alpine's own `alpine` user holds uid 1000 and is deleted to free it.
- **`write_files` runs before `runcmd`**, so golden-recipe files are staged under `/etc/geli/` and installed into `/home/sandbox/` from the setup script, after the user exists. In the *session* document the user is already in the image, so `owner: sandbox:sandbox` works directly.
- **Sessions do not use the bootloader at all.** `--build-image` hands `vmlinuz`, `initramfs` and the kernel cmdline out through a 9p share (the only point where the guest runs as root), and the host keeps them as `geli-vmlinuz` / `geli-initramfs` / `geli-golden.meta` beside the golden image. They are re-extracted on every build; replacing the golden by hand leaves them mismatched. A consequence worth knowing: a kernel updated *inside* a session has no effect.
- **`boot_cmdline` rewrites the image's own command line, it does not invent one.** `root=` and `modules=` are carried over verbatim — they describe how that particular image finds its filesystem, and hardcoding `root=LABEL=/` is exactly the kind of guess that breaks silently on a differently-labelled base.
- **Alpine's cloud image ships a 10-second SYSLINUX boot menu.** Direct boot sidesteps it, and the golden build also sets `TIMEOUT 1` for anyone booting the image by hand. It never appeared in `uptime` or any in-guest measurement — only in wall-clock time, looking like QEMU overhead. **Measuring boot from inside the guest sees neither the firmware nor the bootloader**, which is where the time was.
- **Every network step in the golden build is wrapped in `retry`.** Roughly a fifth of outbound connections drop mid-TLS on some networks, and `set -e` turned any one of them into a failed build.
- **There is no getty on `ttyS0`.** busybox `getty -n` writes an unconditional CRLF before exec'ing the login program, and no flag suppresses it — that was the blank line that used to open every session's stdout. It was long blamed on `login`; capturing the bytes in a pty showed `login -f` alone prints nothing. busybox init opens the tty named in the `inittab` id field as the controlling terminal with sane modes, so the helper runs straight from `inittab`.
- **Autologin is an `/etc/inittab` line plus a login helper**, since Alpine has no systemd. It is baked into the image, so it races the session's cloud-init. init can hand out a shell before `/etc/geli/session` has been written. `GOLDEN_PROFILE` blocks on `cloud-init status --wait` for exactly this reason; if the session file is still missing afterwards it drops to a shell with a pointer to the cloud-init log instead of powering off blind.
- **The host's `~/.claude/.credentials.json` is copied into the guest by default** (`--no-credentials` opts out). The reasoning is the stated threat model: the sandbox exists to keep the agent away from files it was not given, and a credential is not one of those. It is also what makes the agent bill the user's plan instead of API credits. Only that one file is copied — not the rest of `~/.claude`, which holds conversation transcripts and prompt history across every project. Keep it that way.
- **The guest config holds three keys, and that is the whole of it.** `hasCompletedOnboarding`, `customApiKeyResponses` and `projects` are what suppress the first-run prompts — measured: with credentials present but the file absent, the agent still runs onboarding. Copying further state from the host's `~/.claude.json` (machine/user ids, migration version, cached account profile) was tried and reverted: it suppressed no additional prompt and changed no measurable startup time, so it only moved more of the user's data into the guest. `claude_config_holds_only_what_suppresses_the_prompts` fails if the set grows again.
- **Never export an empty `ANTHROPIC_API_KEY`.** Claude Code treats the variable's presence as taking precedence over a claude.ai login, so a blank export silently shadows the forwarded credentials and sends the session to API billing — or to nothing. `build_env_exports` filters empty values for this reason.
- **With no credentials at all, `claude` sits in its first-run login flow forever**, which is why `credential_warning` runs on the host *before* the VM boots rather than letting the user watch a dead console.
- **Disposable VMs lose agent state, so `build_claude_config` seeds `/home/sandbox/.claude.json`.** Without it Claude Code re-runs onboarding every session: approve the API key, then trust the folder, every time. It records `hasCompletedOnboarding`, `hasTrustDialogAccepted` for every mounted workspace, and the last 20 characters of the key under `customApiKeyResponses.approved` (never the whole key). Anything else the agent should treat as already-answered belongs here too.
- **The serial console dictates a generic `TERM` and a fixed 80x24.** `build_terminal_setup` overrides both from the host's terminal in `/etc/geli/env`, which `.bash_profile` sources after login — otherwise agent TUIs render in eight colours in a cramped window. Serial lines carry no SIGWINCH, so the size is a snapshot taken at launch and will not follow a resize.
- **The session script echoes a line before running the user's command.** A command that produces no output would otherwise be indistinguishable from a sandbox that never ran it — the exact failure mode that made the credential bug hard to diagnose.
- **A golden build is verified by a guarded sentinel.** cloud-init does *not* abort `runcmd` on failure, so the build VM only echoes `GOLDEN_OK_MARKER` behind `command -v claude && command -v git && command -v node`. The host greps the console log for it and refuses to publish the image otherwise — the `.building` file is renamed into place only on success.
- `agent_args` is captured with `trailing_var_arg` + `allow_hyphen_values` and re-joined with spaces before going into the guest shell, so arguments are not shell-quoted.
- Workspace names are sanitized to lowercase alphanumerics plus `-`, because they become filenames and the guest hostname.
