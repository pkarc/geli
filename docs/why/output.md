# Why — what geli says, and where it says it

## stderr is geli's, stdout is the command's

`geli claude > out.txt` must capture the command and nothing else. This includes the interactive
prompts: the workspace chooser and the SSH one used to use `println!`, so redirecting the output put
the question in the file and left the user watching a terminal that looked hung, waiting to answer
something they could not see.

Two things deliberately stay on stdout. `display_active_workspaces` is the output of `geli --list`,
not status about it, and has to stay pipeable. `--build-image` runs no command at all, so there is
nothing whose stdout it could be stealing.

## The status block says what changes per invocation

What is mounted, which credential is in play, what is inside the image. Anything identical every
session is noise even when geli writes it.

## The loading line never advances on a timer

It names the phase the guest is actually in — `starting`, `boot`, `network`, `cloud-init`,
`mounts` — and every one of those is a string the guest writes to its own boot console
(`BOOT_PHASES`). A progress indicator that moves on a clock is confidently wrong exactly when the
boot is stuck, which is the only time anyone reads it.

Readiness is a marker `mounts.sh` echoes as its last act. `runcmd` output lands in the boot console
log, which `track_boot` polls for both the phase and the signal. The poll **must** keep its timeout:
a guest whose cloud-init broke will never answer, and the terminal has to be handed over anyway.

Colour is on only when stderr is a terminal and `NO_COLOR` is unset; piped, the same phases print
one line each so a CI log still shows where a boot died.

## A guest that powers itself off early must end the readiness poll

`track_boot` checks `try_wait` on the QEMU child every tick as well as polling the log, because a VM
that died in five seconds used to leave the user watching a spinner for the full 90-second timeout —
and the lockdown's deliberate power-off made that path likely rather than theoretical.

The log is read once more after the child exits: the marker and the power-off can land between two
ticks, and a guest that did its job and then exited is still ready. When it is genuinely not ready,
the last few console lines are printed inline rather than pointing at a file that is about to be
deleted on exit.
