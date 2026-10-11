# Why — images on disk, and how a session boots

## The image chain

`cloud ← base ← layer ← session`. The session overlay is copy-on-write on the invoked agent's
layer, which is an overlay on the base image, which is an overlay on the pristine cloud image.
`resolve_image` builds the layer first if it is missing, so a first run of an agent takes minutes
and every later one seconds.

Only the base image is created with an explicit `SANDBOX_DISK_SIZE` (20G) — Alpine's cloud image is
202 MiB virtual, which `apk add nodejs npm` alone overflows — and everything above it inherits that
size from its backing file.

**Layers are siblings on the base, never stacked.** Running `claude` and later `agy` leaves two
independent layers, and neither VM holds the other's agent. A layer's filenames all derive from
`agent.command` inside `ensure_layer`, so there is no separate key that could disagree with the
agent being installed.

**Rebuilding the base image deletes every layer** (`discard_layers`). A layer's backing file is the
image that was just replaced; keeping it would leave a chain pointing at a file that no longer
exists. A layer's recorded hash also includes its parent's, so a changed recipe invalidates that
layer and anything stacked on it, and nothing else.

**One console log per image, not one per run** (`BASE_LOG_NAME`, `layer_log_name`). They shared
`geli-build.log`, and since each build truncates the file it writes, `--build-image --agents a,b,c`
ended holding only `c`'s console. A failing build survived — it is the last one and the process
stops there — but every success before it was gone, and CI uploaded the remainder as the run's only
evidence. `discard_layers` sweeps the layer logs along with their images, which is why a layer's
log has to keep the `geli-layer-` prefix.

## Two things that were rejected, with the machinery removed

**Several agents in one VM** — `docs/plans/05-varios-agentes-por-vm.md`. `resolve_image` takes
`Option<&Agent>` and is straight-line; it used to be `resolve_chain` walking a variable-length chain
of prefixes, with `layer_key` sorting and deduping agent names into `a+b` keys. Nothing ever called
it with more than one agent, so it was removed with the decision rather than left as generality
that looks exercised and is not.

**Reusing a running VM instead of starting one** — `docs/plans/04-reutilizar-vm.md`. Credential
isolation, the egress policy and the mounted set are per-VM properties by construction; sharing a VM
gives up all three to save ten seconds the base image and layers already saved. Reuse would only be
safe when the workspace, agent and egress mode all match, which is also when nobody wants it.

## Builds

**Every build is verified by a guarded sentinel.** cloud-init does *not* abort `runcmd` on failure,
so a build VM only echoes `BUILD_OK_MARKER` behind a real check: the toolchain for the base image,
`command -v <binary>` for an agent layer. `run_build_vm` greps the console log for it and returns
`None` otherwise, and the `.building` file is renamed into place only on success.

**Every network step in a build is wrapped in `retry`.** The helper lives in `src/guest/retry.sh`
and is substituted into both the base recipe and every agent layer, so the two cannot drift. Roughly
a fifth of outbound connections drop mid-TLS on some networks, and `set -e` turned any one of them
into a failed build.

**Deleting files in the guest does not shrink the qcow2.** Blocks written during a build stay
allocated. The build drive runs with `discard=unmap` and the recipe ends in `fstrim`, which took the
image from 1.9 GB to 922 MB. Anything that writes-then-deletes during provisioning depends on this.

**Node packages that ship per-platform binaries need pruning.** `opencode-ai` installs glibc, musl
and "baseline" variants at ~180 MB each and hardlinks the right one into `bin/`; deleting the other
three took it from 728 MB to 187 MB with the CLI still working.

## Concurrency, and the build lock

Several sessions can run at once, and layer building is the part that had to be made safe.
Everything per-session is keyed by the host pid (`/tmp/sandbox-session-<pid>.qcow2`, the share dir,
the cloud-init instance-id) and the proxy binds an ephemeral port, so concurrency mostly fell out
for free — measured, three concurrent `claude` sessions at 373/351/343 MB RSS.

But layers are built on *first use*, so two shells starting the same agent raced for the same
`.building` path, and `ensure_layer` even began by deleting it: the second run wiped the first's
half-built overlay and died with a bare `Error: Os { code: 2 }`. `try_build_lock` is an `O_EXCL`
file per image; the pending overlay is pid-keyed too. The lock holds its own pid because a failed
build leaves via `process::exit`, which skips the guard's `Drop` — so `lock_holder_alive` is what
stops a killed build from blocking every later one. An unparseable lock counts as held, which is the
cautious direction.

## Boot

**Sessions do not use the bootloader at all.** `--build-image` hands `vmlinuz`, `initramfs` and the
kernel cmdline out through a 9p share (the only point where the guest runs as root), and the host
keeps them as `geli-vmlinuz` / `geli-initramfs` / `geli-base.meta` beside the base image. They are
re-extracted on every base build; replacing the image by hand leaves them mismatched. A consequence
worth knowing: a kernel updated *inside* a session has no effect.

**`boot_cmdline` rewrites the image's own command line, it does not invent one.** `root=` and
`modules=` are carried over verbatim — they describe how that particular image finds its filesystem,
and hardcoding `root=LABEL=/` is exactly the kind of guess that breaks silently on a
differently-labelled base.

**Alpine's cloud image ships a 10-second SYSLINUX boot menu.** Direct boot sidesteps it (~6s of the
old session time, along with SeaBIOS and iPXE), and the base build also sets `TIMEOUT 1` for anyone
booting the image by hand. It never appeared in `uptime` or any in-guest measurement — only in
wall-clock time, looking like QEMU overhead. **Measuring boot from inside the guest sees neither the
firmware nor the bootloader**, which is where the time was.

**KVM is detected by opening `/dev/kvm`, not by stat'ing it.** On GitHub's runners the device exists
but is unusable until a udev rule puts the user in the `kvm` group, and an existence check would
call that host accelerated and then fail at launch. `-enable-kvm -cpu host` and `-cpu max` move
together, because `-cpu host` is KVM-only and QEMU refuses it under emulation. Falling back warns
rather than exits — measured at 10s accelerated against 99s emulated for the same session.

## The memory ceiling is not a reservation

That was the roadmap item's wrong premise. "The guest uses 476 MB, so trim the 4 GB" assumed the
4 GB was being spent; Linux hands QEMU pages only as the guest touches them.

The real cost of a high ceiling is page cache: a Linux guest fills what it is given and QEMU's RSS
never returns it. Measured peak RSS with an agent reading a file — 728 MB at 4 GiB, 635 at 2, 588 at
1 — so the peak tracks the ceiling, not the workload. 2 GiB is the default because 1 GiB put a
trivial task at 57% of the ceiling, and guessing low gets the guest OOM-killed mid-session.
`GELI_MEMORY` overrides it.
