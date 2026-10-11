# Why — what the guest is told to be

## Three cloud-init documents, and the split is the whole performance story

`build_base_cloud_init` is baked once by `--build-image` and holds everything static *and shared*:
the toolchain, the sandbox user, the autologin inittab line and `LOGIN_PROFILE`.
`build_layer_cloud_init` is baked once per agent and holds exactly one agent's install.
`build_cloud_init` runs per session and holds only what depends on the workspace: mounts, env,
credentials and the command.

Anything slow belongs in one of the first two — putting it in the session document is what made
boots take 4.5 minutes instead of nine seconds; and anything shared belongs in the base rather than
repeated in every layer.

Keeping the generators pure (`build_mount_script`, `build_cloud_init`, `build_base_cloud_init`,
`build_layer_cloud_init`, `build_env_exports`, `indent_block`) is what makes the guest config
testable without booting a VM — add tests there rather than debugging through the serial console.

## The cloud-init YAML has a deliberately fixed shape

Everything variable (mounts, env, the user's command) is injected as a literal block scalar via
`indent_block`, so the document's structure never depends on the number of workspace directories.

An earlier version built `runcmd` entries by string-replacing newlines; the indentation didn't match
and the YAML never parsed, which fails *silently* — the VM boots fine and the command simply never
runs. If you add anything variable here, put it in a `content: |` block, not in a list item, and
extend `cloud_init_parses`.

**`write_files` runs before `runcmd`**, so base-recipe files are staged under `/etc/geli/` and
installed into `/home/sandbox/` from the setup script, after the user exists. In the *session*
document the user is already in the image, so `owner: sandbox:sandbox` works directly.

## The `sandbox` user is created in the base setup script, not by cloud-init

Alpine's users module cannot set an explicit `uid` and fails the whole module when asked to —
silently, leaving no user, which then breaks `write_files` entries that specify an owner, which
leaves `/etc/geli/mounts.sh` unwritten, which leaves the workspace unmounted.

The uid must match the host's: files arrive over 9p owned by the host user, so a mismatch leaves the
agent able to read the project but not write to it. Alpine's own `alpine` user holds uid 1000 and is
deleted to free it.

## Login, and the two tty surprises

**Autologin is an `/etc/inittab` line plus a login helper**, since Alpine has no systemd. It is
baked into the image, so it races the session's cloud-init: init can hand out a shell before
`/etc/geli/session` has been written. `GOLDEN_PROFILE` blocks on `cloud-init status --wait` for
exactly this reason; if the session file is still missing afterwards it drops to a shell with a
pointer to the cloud-init log instead of powering off blind.

**There is no getty on `ttyS0`.** busybox `getty -n` writes an unconditional CRLF before exec'ing
the login program, and no flag suppresses it — that was the blank line that used to open every
session's stdout. It was long blamed on `login`; capturing the bytes in a pty showed `login -f`
alone prints nothing. busybox init opens the tty named in the `inittab` id field as the controlling
terminal with sane modes, so the helper runs straight from `inittab`.

**The command goes in `.bash_profile`, never `.bashrc`.** `.bashrc` runs for every shell, so a
subshell spawned by the agent would re-run the command and `poweroff` mid-session. The autologin
helper runs `login -f`, which gives a login shell, which is what reads `.bash_profile`.

**An agent layer is built on an image whose autologin already works**, so the login profile runs
during the build too and leaves `/tmp/.geli-session-active` behind. Baked into the layer, that flag
sends every real session down the nested-shell branch: the command never runs and the VM sits there
until the readiness poll times out. `layer.sh` removes it explicitly, and a test asserts it does.

## The terminal

**The serial console dictates a generic `TERM` and a fixed 80x24.** `build_terminal_setup` overrides
both from the host's terminal in `/etc/geli/env`, which `.bash_profile` sources after login —
otherwise agent TUIs render in eight colours in a cramped window. Serial lines carry no SIGWINCH, so
the size is a snapshot taken at launch and will not follow a resize.

**The session script echoes a line before running the user's command.** A command that produces no
output would otherwise be indistinguishable from a sandbox that never ran it — the exact failure
mode that made the credential bug hard to diagnose.
