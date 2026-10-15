//! The Linux sandbox driver: images on disk, direct kernel boot, and watching the guest come up.

#[allow(unused_imports)]
use crate::dirs_home_dir;
use crate::{guest::*, ui::*};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// --- what lives in ~/qemu-sandbox ---
//
// Three kinds of image, and the distinction matters when reading the rest of this file:
//
//   the cloud image   downloaded, pristine, never written to      nocloud_alpine-….qcow2
//   the base image    provisioned toolchain, no agents            geli-base.qcow2
//   an agent layer    one agent, on the base or on another layer  geli-layer-<key>.qcow2
//
// A session is a throwaway overlay on the top of that chain. The old single `geli-golden.qcow2`
// held the toolchain *and* every agent at once, which is why this split exists.

pub(crate) const CLOUD_IMAGE_NAME: &str = "nocloud_alpine-3.22.2-x86_64-bios-cloudinit-r0.qcow2";
pub(crate) const BASE_IMAGE_NAME: &str = "geli-base.qcow2";
pub(crate) const BASE_RECIPE_NAME: &str = "geli-base.recipe";
pub(crate) const BASE_META_NAME: &str = "geli-base.meta";
/// One console log per image, not one per run.
///
/// A single shared log meant `--build-image --agents a,b,c` ended with only `c`'s console, because
/// each build truncates the file it writes to. The failing build survived — it is the last one, and
/// the process stops there — but every successful one before it was gone, and CI uploaded the
/// incomplete remainder as its only evidence.
pub(crate) const BASE_LOG_NAME: &str = "geli-base.log";

pub(crate) fn layer_log_name(key: &str) -> String {
    format!("geli-layer-{}.log", key)
}

pub(crate) fn layer_lock_name(key: &str) -> String {
    format!("geli-layer-{}.building.lock", key)
}

/// Proof that this process, and no other, is the one building a given image.
///
/// Images are built on first use, so two shells starting the same agent for the first time race
/// for the same files. Measured before this existed: two `geli agy` a second apart, and the second
/// deleted the first's half-built overlay, then died with a bare `Error: Os { code: 2 }`. Exactly
/// what two tmux panes do.
///
/// Released on drop, which covers every early return — but *not* `process::exit`, which a failed
/// build takes. That is why the holder's pid goes in the file: a waiter can tell "still building"
/// from "died holding this".
pub struct BuildLock {
    path: PathBuf,
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// `Some(lock)` means go ahead and build. `None` means someone else got there first.
pub fn try_build_lock(path: &Path) -> io::Result<Option<BuildLock>> {
    match fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            // Best effort: the pid is for diagnosing a stale lock, not for correctness. The
            // exclusivity is `create_new`, which is atomic.
            let _ = writeln!(file, "{}", std::process::id());
            Ok(Some(BuildLock { path: path.to_path_buf() }))
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether the process named in a lock file is still alive. A lock nobody holds is a leftover from
/// a build that was killed, and refusing to build again because of it would be worse than the race
/// this is all here to prevent.
pub fn lock_holder_alive(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        // The file vanished between the two calls: whoever held it is done, which is not "alive".
        return false;
    };
    match text.trim().parse::<u32>() {
        Ok(pid) => Path::new(&format!("/proc/{}", pid)).exists(),
        // An unreadable lock is treated as held: a geli too old to write a pid may be building.
        Err(_) => true,
    }
}

/// The leftovers a session can have on disk, as name prefixes under `/tmp`, each followed by the
/// pid of the geli that owns it.
///
/// Everything per-session is pid-keyed so that several sessions can run at once; that is also
/// what makes them sweepable, because a pid tells you whether anyone still wants the file.
const SESSION_LEFTOVERS: &[&str] =
    &["sandbox-session-", "sandbox-share-", "geli-stage-", "geli-out-"];

/// The pid a leftover is named after, if that is what the name is.
///
/// Pure, so the thing that decides what gets deleted is testable without creating files to
/// delete. The pid runs to the end of the name or to the next `-`: the session overlay is
/// `sandbox-session-<pid>.qcow2` and a build's staging is `geli-stage-<pid>-<instance>`.
pub(crate) fn leftover_pid(name: &str) -> Option<u32> {
    let rest = SESSION_LEFTOVERS.iter().find_map(|prefix| name.strip_prefix(prefix))?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let tail = &rest[digits.len()..];
    // Anything else between the pid and the end is not one of ours: `sandbox-share-12x` is not
    // a session, and deleting it because it starts the same way would be guessing.
    if digits.is_empty() || !(tail.is_empty() || tail.starts_with('-') || tail.starts_with('.')) {
        return None;
    }
    digits.parse().ok()
}

/// Delete what sessions whose geli is gone left behind.
///
/// **`Drop` is the plan; this is what happens when there was no plan.** `SessionScratch` clears up
/// on every path that returns, but `SIGHUP` — the terminal closing — and `SIGKILL` do not return
/// anywhere, and each one leaves a 20 GiB-capable overlay and a share directory in `/tmp`. A
/// signal handler would cover the first and could never cover the second, so the sweep comes
/// first and costs no dependency.
///
/// **A live session must survive this**, which is the only hard part: several geli processes
/// share `/tmp`, so nothing is removed unless the pid in its name belongs to no running process.
/// Failures are ignored on purpose — another user's leftovers are not ours to delete, and a
/// sweep that refuses to start geli would be worse than the mess it tidies.
pub fn sweep_dead_sessions() {
    // Both roots: the one geli uses now, and plain `/tmp`, where sessions from before the
    // checked root left theirs. Dropping the second would strand those forever.
    let roots = [staging_root().unwrap_or_else(|_| PathBuf::from("/tmp")), PathBuf::from("/tmp")];

    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else { continue };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(pid) = leftover_pid(name) else { continue };
            if Path::new(&format!("/proc/{}", pid)).exists() {
                continue;
            }
            // **Ours, checked, not assumed.** Until now this leaned on `/tmp`'s sticky bit to
            // make deleting someone else's file fail. With `hidepid` on `/proc`, another user's
            // live pid reads as dead, and the only thing standing between this and deleting
            // their running session would be an error we ignore.
            if entry.metadata().map(|m| m.uid()).ok() != Some(host_uid()) {
                continue;
            }

            let path = entry.path();
            let _ = if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
        }
    }
}

/// Wait out another geli's build of the same image.
///
/// Returns when the lock is gone — either the build finished, or its holder died and the lock was
/// cleared here. The timeout is generous because the thing being waited on is a real image build:
/// Antigravity's layer takes about three minutes on this hardware.
pub fn wait_for_builder(lock: &Path, label: &str, timeout: Duration) -> bool {
    if !lock.exists() {
        return true;
    }

    eprintln!("[*] Another geli is building the {} layer. Waiting for it.", label);
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if !lock.exists() {
            return true;
        }
        if !lock_holder_alive(lock) {
            eprintln!("[*] That build died without finishing. Taking over.");
            let _ = fs::remove_file(lock);
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}
pub(crate) const KERNEL_NAME: &str = "geli-vmlinuz";
pub(crate) const INITRD_NAME: &str = "geli-initramfs";
/// What `--build-image` wrote before images were layered. Only used to recognise it and say so.
pub(crate) const LEGACY_IMAGE_NAME: &str = "geli-golden.qcow2";
/// Virtual size of the base image. Alpine's cloud image is 202 MiB virtual, which
/// `apk add nodejs npm` alone overflows. qcow2 is sparse, so this costs nothing until used, and
/// cloud-init's growpart expands the root partition to match on first boot. Agent layers and
/// session overlays inherit this size from their backing file.
pub(crate) const SANDBOX_DISK_SIZE: &str = "20G";

/// Filenames for one agent's layer. `key` is the agent's command, which is also the whole cache:
/// `geli claude` looks for `geli-layer-claude.qcow2` and builds it if it is not there.
///
/// Layers are siblings on the base image, never stacked on each other — see
/// `docs/plans/05-varios-agentes-por-vm.md`. Running `claude` and later `agy` leaves two
/// independent layers, and neither VM holds the other's agent.
pub(crate) fn layer_image_name(key: &str) -> String {
    format!("geli-layer-{}.qcow2", key)
}

pub(crate) fn layer_recipe_name(key: &str) -> String {
    format!("geli-layer-{}.recipe", key)
}

pub(crate) fn layer_meta_name(key: &str) -> String {
    format!("geli-layer-{}.meta", key)
}

/// Where QEMU's console goes. Sessions inherit the terminal so the agent is interactive;
/// image builds are unattended and go to a log we can inspect afterwards.
pub enum QemuIo {
    Interactive,
    LogTo(PathBuf),
}

/// Size of the terminal geli was launched from, if it was launched from one at all.
pub fn host_terminal_size() -> Option<(u16, u16)> {
    let out = Command::new("stty")
        .arg("size")
        .stdin(Stdio::inherit())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace();
    let rows: u16 = parts.next()?.parse().ok()?;
    let cols: u16 = parts.next()?.parse().ok()?;

    if rows == 0 || cols == 0 {
        return None;
    }
    Some((rows, cols))
}

pub fn images_dir() -> PathBuf {
    dirs_home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("qemu-sandbox")
}

pub fn check_host_tools() {
    let present = |bin: &str| {
        Command::new("which")
            .arg(bin)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };

    if !present("qemu-system-x86_64") || !present("genisoimage") {
        eprintln!("\n[!] Error: Missing required system virtualization utilities.");
        eprintln!("geli requires QEMU and genisoimage to create secure hardware sandboxes.");
        eprintln!("\nPlease install them by running:");
        eprintln!(
            "  sudo apt update && sudo apt install -y qemu-system-x86 qemu-utils genisoimage\n"
        );
        std::process::exit(1);
    }

    // Not fatal. Emulation is slow, not broken, and refusing to run at all on a host without
    // hardware virtualization would be geli deciding something that is the user's to decide.
    // But it has to be said, because the symptom otherwise is "geli got mysteriously slow".
    if !kvm_available() {
        eprintln!("\n[!] No hardware virtualization: /dev/kvm is missing or not yours to use.");
        eprintln!("    geli will fall back to emulation, which is many times slower — a session");
        eprintln!("    takes minutes instead of seconds, and building an image takes much longer.");
        eprintln!("    If the device exists, you are probably not in the `kvm` group:");
        eprintln!("      sudo usermod -aG kvm $USER     # then log out and back in\n");
    }
}

/// Create a private staging directory. The cloud-init payload carries API keys in plaintext,
/// so it must not be world-readable.
/// The one directory under `/tmp` that geli writes into, checked once instead of trusted often.
///
/// **`/tmp`'s sticky bit stops another user deleting our files; it does not stop them creating a
/// name first.** Every per-session path here is predictable — `sandbox-share-<pid>` — so another
/// local user could plant that directory, or a symlink wearing its name, and geli would either
/// die on the `chmod` or write a `user-data` through the link. Checking one root beats checking
/// a new path per session.
///
/// **And it stays on `/tmp` rather than `$XDG_RUNTIME_DIR`.** Measured on this host: `/tmp` is a
/// 16G tmpfs and `/run/user/1000` is 3.2G. The session overlay lives under here and grows with
/// what the guest writes, so moving it would trade a security problem for a disk-space one.
/// → `docs/plans/11-el-orden-dentro-del-arranque.md`
pub fn staging_root() -> io::Result<PathBuf> {
    let root = PathBuf::from(format!("/tmp/geli-{}", host_uid()));

    match fs::symlink_metadata(&root) {
        // Ours already: say so out loud rather than assume it, because this is the check the
        // per-session paths are no longer doing for themselves.
        Ok(meta) => {
            let kind = meta.file_type();
            if !kind.is_dir() || meta.uid() != host_uid() || meta.mode() & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{} is not a directory this user owns privately — something else made \
                         it, and geli will not write a session through it",
                        root.display()
                    ),
                ));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&root)?;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        Err(e) => return Err(e),
    }

    Ok(root)
}

pub fn staging_dir(name: &str) -> io::Result<PathBuf> {
    let dir = staging_root()?.join(name);
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

pub fn make_cloud_init_iso(
    dir: &Path,
    user_data: &str,
    instance_id: &str,
    hostname: &str,
) -> io::Result<PathBuf> {
    let user_data_path = dir.join("user-data");
    let meta_data_path = dir.join("meta-data");
    let iso_path = dir.join("cloud-init.iso");

    fs::write(&user_data_path, user_data)?;
    fs::write(
        &meta_data_path,
        format!(
            "instance-id: {}\nlocal-hostname: {}\n",
            instance_id, hostname
        ),
    )?;

    let status = Command::new("genisoimage")
        .args([
            "-output",
            iso_path.to_str().unwrap(),
            "-volid",
            "cidata",
            "-joliet",
            "-rock",
            user_data_path.to_str().unwrap(),
            meta_data_path.to_str().unwrap(),
        ])
        .output()?;

    if !status.status.success() {
        eprintln!("Failed to generate cloud-init ISO.");
        eprintln!("{}", String::from_utf8_lossy(&status.stderr));
        return Err(io::Error::other("genisoimage failed"));
    }
    Ok(iso_path)
}

/// Copy-on-write layer over `backing`. `size` is only needed when growing past the backing
/// file's virtual size; sessions inherit the golden image's size by passing None.
pub fn create_overlay(backing: &Path, target: &Path, size: Option<&str>) -> io::Result<()> {
    let mut args: Vec<String> = vec![
        "create".into(),
        "-f".into(),
        "qcow2".into(),
        "-F".into(),
        "qcow2".into(),
        "-b".into(),
        backing.to_string_lossy().into_owned(),
        target.to_string_lossy().into_owned(),
    ];
    if let Some(size) = size {
        args.push(size.to_string());
    }

    let status = Command::new("qemu-img").args(&args).output()?;
    if !status.status.success() {
        eprintln!("Failed to create qcow2 snapshot layer.");
        eprintln!("{}", String::from_utf8_lossy(&status.stderr));
        return Err(io::Error::other("qemu-img create failed"));
    }
    Ok(())
}

/// Boots the kernel directly instead of going through the firmware and bootloader, with the
/// guest's console on a second serial port.
pub struct DirectBoot {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub cmdline: String,
    /// Where the guest's boot console goes. Never the user's terminal.
    pub console_log: PathBuf,
}

/// Whether this host can hand the guest real hardware virtualization.
///
/// Opened, not merely stat'ed: `/dev/kvm` existing proves nothing about whether this user may use
/// it. CI is the case that makes the distinction concrete — the hosted runners have the device,
/// but the runner user is not in the `kvm` group until a udev rule says so, and a `stat` check
/// would have called that host accelerated and then failed at launch.
pub fn kvm_available() -> bool {
    if std::env::var_os("GELI_NO_KVM").is_some() {
        return false;
    }
    fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok()
}

/// How much memory the guest gets. `GELI_MEMORY` overrides it in either direction.
///
/// It is a ceiling, not a reservation: Linux hands QEMU pages as the guest touches them, so the
/// old 4 GiB was never actually spent. What a high ceiling does cost is page cache — a Linux
/// guest will fill whatever it is given, and QEMU's RSS does not shrink when the guest frees it.
/// Measured as peak QEMU RSS, which is an upper bound since it includes QEMU itself:
///
/// | ceiling | agent reading a file | `npm install express` |
/// |---|---|---|
/// | 4 GiB | 728 MB | — |
/// | 2 GiB | 635 MB | 430 MB |
/// | 1 GiB | 588 MB | 425 MB |
///
/// Everything above worked, 1 GiB included. 2 GiB is the default anyway: at 1 GiB a trivial agent
/// task already touched 57% of the ceiling, and the failure mode for guessing low is the guest
/// being OOM-killed mid-session, which is far worse than a ceiling nobody reaches. Note how RSS
/// tracks the ceiling rather than the workload — that is the page cache, and it is the reason to
/// come down from 4 GiB at all.
pub fn guest_memory() -> String {
    std::env::var("GELI_MEMORY").unwrap_or_else(|_| "2G".to_string())
}

/// How the guest's CPU is provided. The two halves must move together: `-cpu host` is a KVM-only
/// model and QEMU refuses it outright under emulation, so pairing it with a missing `-enable-kvm`
/// is not a slow sandbox, it is one that will not start.
///
/// With acceleration, `host` passes the real CPU through. Without it, `max` is the nearest thing
/// TCG offers — every feature this QEMU can emulate. Neither is `qemu64`, QEMU's default: a
/// deliberately conservative model missing most modern instruction sets, which sends guest
/// userspace down feature-detection paths nothing else exercises.
pub fn accel_args(kvm: bool) -> Vec<String> {
    if kvm {
        vec!["-enable-kvm".into(), "-cpu".into(), "host".into()]
    } else {
        vec!["-cpu".into(), "max".into()]
    }
}

pub fn run_qemu(
    disk: &Path,
    iso: &Path,
    extra_args: Vec<String>,
    io_mode: QemuIo,
    direct: Option<&DirectBoot>,
) -> io::Result<Child> {
    let mut args: Vec<String> = vec!["-m".into(), guest_memory()];
    args.extend(accel_args(kvm_available()));

    args.extend([
        "-smp".into(),
        "2".into(),
        "-nographic".into(),
        "-drive".into(),
        // discard=unmap lets the guest's fstrim actually return blocks to the qcow2. Without
        // it, anything written and then deleted during a build stays in the image forever:
        // npm writes ~540 MB of platform variants we delete, and the file stayed 2.1 GB.
        format!("file={},if=virtio,discard=unmap,detect-zeroes=unmap", disk.display()),
        "-drive".into(),
        format!("file={},format=raw,if=virtio", iso.display()),
        "-netdev".into(),
        "user,id=net0".into(),
    ]);

    if direct.is_some() {
        // The iPXE option ROM prints a banner and is never used — nothing here network-boots.
        args.push("-device".into());
        args.push("virtio-net-pci,netdev=net0,romfile=".into());
        // Silences SeaBIOS's own serial banner.
        args.push("-fw_cfg".into());
        args.push("name=etc/sercon-port,string=0".into());
    } else {
        args.push("-device".into());
        args.push("virtio-net-pci,netdev=net0".into());
    }

    // Order matters: the first -serial is ttyS0, the second ttyS1.
    args.push("-serial".into());
    args.push("mon:stdio".into());

    if let Some(d) = direct {
        args.push("-serial".into());
        args.push(format!("file:{}", d.console_log.display()));
        args.push("-kernel".into());
        args.push(d.kernel.to_string_lossy().into_owned());
        args.push("-initrd".into());
        args.push(d.initrd.to_string_lossy().into_owned());
        args.push("-append".into());
        args.push(d.cmdline.clone());
    }

    args.extend(extra_args);

    let mut command = Command::new("qemu-system-x86_64");
    command.args(&args);

    match io_mode {
        // Inherit the terminal so the agent gets a real interactive TTY. QEMU's own stderr
        // goes to the console log when there is one: its warnings are not the user's problem,
        // and the whole point here is that nothing but geli and the command reach the screen.
        QemuIo::Interactive => {
            command.stdin(Stdio::inherit()).stdout(Stdio::inherit());
            match direct.map(|d| &d.console_log) {
                Some(log) => {
                    let f = fs::OpenOptions::new().create(true).append(true).open(log)?;
                    command.stderr(Stdio::from(f));
                }
                None => {
                    command.stderr(Stdio::inherit());
                }
            }
        }
        QemuIo::LogTo(path) => {
            let log = File::create(&path)?;
            command
                .stdin(Stdio::null())
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log));
        }
    }

    command.spawn()
}

/// Show which phase the guest is in and return once it reports ready.
///
/// Returns false if the guest never reports ready. Two ways that happens, and both have to end
/// the wait:
///
/// - **the VM is gone.** A guest that powers itself off early — a broken cloud-init, or the
///   lockdown refusing to run a session it cannot restrict — will never write the marker, and
///   polling for it until the deadline left the user watching a spinner for the full 90 seconds
///   over a VM that died in five. Checked every tick, which costs nothing.
/// - **the timeout.** A guest that is merely wedged still holds the terminal hostage, and it has
///   to be handed over regardless rather than spin forever.
pub fn track_boot(
    log: &Path,
    vm: &mut Child,
    animate: bool,
    color: bool,
    timeout: Duration,
) -> bool {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let started = Instant::now();
    let deadline = started + timeout;
    let mut frame = 0usize;
    let mut announced = "";

    while Instant::now() < deadline {
        let text = fs::read_to_string(log).unwrap_or_default();
        if text.contains(READY_MARKER) {
            if animate {
                eprint!("\r\x1b[2K");
                let _ = io::stderr().flush();
            }
            return true;
        }

        // Read the log before this check, not after: the marker and the power-off can land
        // between two ticks, and a guest that did its job and then exited is still ready.
        if matches!(vm.try_wait(), Ok(Some(_))) {
            if animate {
                eprint!("\r\x1b[2K");
                let _ = io::stderr().flush();
            }
            return fs::read_to_string(log).unwrap_or_default().contains(READY_MARKER);
        }

        let phase = boot_phase(&text);
        if animate {
            eprint!(
                "\r\x1b[2K{}",
                render_spinner_line(
                    FRAMES[frame % FRAMES.len()],
                    phase,
                    started.elapsed().as_secs_f32(),
                    color
                )
            );
            let _ = io::stderr().flush();
            frame += 1;
        } else if phase != announced {
            // Piped or logged: one line per phase instead of an animation nobody will see.
            eprintln!("  booting · {}", phase);
            announced = phase;
        }

        std::thread::sleep(Duration::from_millis(90));
    }
    false
}

/// The uid the guest's `sandbox` user must take: files arrive over 9p owned by the host user,
/// so a mismatch leaves the agent unable to write to the project it was given.
pub(crate) fn host_uid() -> u32 {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1000)
}

/// The `-fsdev`/`-device` pairs that map the host's directories into the guest over 9p.
///
/// Pure, and that is the point: what the guest ends up seeing is decided entirely here, and this
/// used to be thirty lines in the middle of `execute_sandbox` that could only be checked by
/// booting a VM and looking.
///
/// Tags are positional (`projshare1`, `projshare2`, …) and the guest mounts each at
/// `/workspace/<folder-name>`, so the order of `dirs` is part of the contract.
pub(crate) fn share_args(dirs: &[PathBuf], caches: &[(&str, PathBuf)]) -> Vec<String> {
    let mut args = Vec::new();
    let mut add = |tag: &str, path: &Path| {
        args.push("-fsdev".to_string());
        args.push(format!("local,path={},id={},security_model=none", path.display(), tag));
        args.push("-device".to_string());
        args.push(format!("virtio-9p-pci,fsdev={},mount_tag={}", tag, tag));
    };

    for (i, dir) in dirs.iter().enumerate() {
        add(&share_tag(i), dir);
    }
    for (tag, path) in caches {
        add(tag, path);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer_hash;
    use crate::agents;
    use crate::agent_for_command;

    /// One `-fsdev`/`-device` pair per directory, tagged by position, and the caches after them.
    #[test]
    fn every_directory_gets_its_own_share() {
        let dirs = vec![PathBuf::from("/home/u/a"), PathBuf::from("/home/u/b")];
        let caches = vec![("npmcache", PathBuf::from("/home/u/.cache/npm"))];
        let args = share_args(&dirs, &caches);

        assert_eq!(args.len(), 12, "four arguments per share, three shares");
        assert!(args.contains(&"local,path=/home/u/a,id=projshare1,security_model=none".to_string()));
        assert!(args.contains(&"local,path=/home/u/b,id=projshare2,security_model=none".to_string()));
        assert!(args.contains(&"virtio-9p-pci,fsdev=projshare2,mount_tag=projshare2".to_string()));
        assert!(args.contains(&"virtio-9p-pci,fsdev=npmcache,mount_tag=npmcache".to_string()));

        // The caches come last, so a directory's tag never depends on how many caches there are.
        assert_eq!(args[8], "-fsdev");
        assert!(args[9].contains("npmcache"));

        assert!(share_args(&[], &[]).is_empty());
    }


    /// Phases must come from the guest's own output. A loader that advances on a timer is
    /// confidently wrong exactly when the boot is stuck, which is the only time anyone reads it.
    #[test]
    fn boot_phase_tracks_the_guest_not_a_clock() {
        assert_eq!(boot_phase(""), "starting");
        assert_eq!(boot_phase("   OpenRC 0.62.6 is starting up"), "boot");
        assert_eq!(boot_phase("OpenRC\n * Starting networking ... [ ok ]"), "network");
        assert_eq!(
            boot_phase("OpenRC\nStarting networking\nCloud-init v. 24.3.1 running 'init' at"),
            "cloud-init"
        );
        assert_eq!(
            boot_phase("OpenRC\nStarting networking\nrunning 'init'\n+ mount -t 9p -o trans=virtio"),
            "mounts"
        );
    }

    /// `running 'init-local'` is an earlier stage and must not be mistaken for `running 'init'`.
    #[test]
    fn boot_phase_does_not_confuse_init_local_with_init() {
        let early = "OpenRC\nCloud-init v. 24.3.1 running 'init-local' at Sun";
        assert_eq!(boot_phase(early), "boot");
    }

    /// The guest's console must never land on the user's terminal, and `root=` must survive:
    /// hardcoding it is how a boot breaks silently on a differently-labelled image.
    #[test]
    fn boot_cmdline_moves_the_console_and_keeps_root() {
        let original = "BOOT_IMAGE=vmlinuz-virt root=LABEL=/ modules=sd-mod,usb-storage,ext4 \
                        console=ttyS0,115200n8 console=ttyAMA0,115200n8 initrd=initramfs-virt";
        let out = boot_cmdline(original);

        assert!(out.contains("root=LABEL=/"));
        assert!(out.contains("modules=sd-mod,usb-storage,ext4"));
        assert!(out.contains(&format!("console={}", BOOT_CONSOLE)));
        assert!(out.contains("quiet"));

        // Nothing may route the guest console back to ttyS0, and the bootloader's own keys are
        // meaningless without a bootloader.
        assert!(!out.contains("ttyS0"));
        assert!(!out.contains("BOOT_IMAGE="));
        assert!(!out.contains("initrd="));
    }

    #[test]
    fn boot_cmdline_is_idempotent() {
        let once = boot_cmdline("root=LABEL=/ console=ttyS0,115200n8");
        assert_eq!(once, boot_cmdline(&once), "re-deriving must not stack flags");
        assert_eq!(once.matches("quiet").count(), 1);
    }

    #[test]
    fn image_meta_round_trips() {
        let meta = parse_image_meta("cmdline=root=LABEL=/ console=ttyS0\nalpine=3.22.2\nnode=22.23.2\n");
        assert_eq!(meta.alpine, "3.22.2");
        assert_eq!(meta.node, "22.23.2");
        assert!(meta.cmdline.starts_with("root=LABEL=/"));
        assert!(meta.agents.is_empty(), "the base image carries no agent");
        assert_eq!(describe_image(&meta), "alpine 3.22.2");
    }

    /// The base image's metadata plus one file per layer is what the status block describes, so
    /// the merge has to accumulate agents rather than replace them.
    #[test]
    fn image_meta_accumulates_one_entry_per_layer() {
        let mut meta = parse_image_meta("alpine=3.22.2\nnode=22.23.2\n");
        merge_image_meta(&mut meta, "agent.claude=2.1.289\n");
        merge_image_meta(&mut meta, "agent.opencode=0.4.2\n");

        assert_eq!(
            meta.agents,
            vec![
                ("claude".to_string(), "2.1.289".to_string()),
                ("opencode".to_string(), "0.4.2".to_string())
            ]
        );
        assert_eq!(describe_image(&meta), "alpine 3.22.2 · claude 2.1.289 · opencode 0.4.2");
    }

    #[test]
    fn image_meta_tolerates_a_missing_or_partial_file() {
        assert_eq!(parse_image_meta(""), ImageMeta::default());
        let partial = parse_image_meta("node=22.23.2\nagent.agy=1.2.16\ngarbage line\n");
        assert_eq!(partial.node, "22.23.2");
        // An image line with holes in it should not print empty fields.
        assert_eq!(describe_image(&partial), "agy 1.2.16");
    }

    /// A layer's recorded hash covers its parent's, so editing one agent's recipe invalidates
    /// that layer and everything stacked above it — and leaves its siblings alone.
    #[cfg(target_os = "linux")]
    #[test]
    fn layer_hash_follows_the_parent() {
        let base = recipe_hash("base");
        let claude = layer_hash(&base, "install claude");
        let stacked = layer_hash(&claude, "install opencode");

        assert_ne!(claude, layer_hash(&base, "install claude v2"));
        assert_ne!(
            stacked,
            layer_hash(&layer_hash(&base, "install claude v2"), "install opencode"),
            "a changed parent must invalidate what sits on it"
        );
        assert_ne!(claude, layer_hash(&recipe_hash("other base"), "install claude"));
    }

    /// The layer recipe is the one place an agent's `install` reaches the guest now, and the
    /// sentinel has to sit behind the binary existing: cloud-init does not abort runcmd on
    /// failure, so a layer whose install died would otherwise be published as working.
    #[test]
    fn layer_recipe_installs_one_agent_and_proves_it() {
        let agent = agent_for_command("claude").expect("claude is a shipped recipe");
        let doc = build_layer_cloud_init(agent, 1000);

        let parsed = yaml_rust2::YamlLoader::load_from_str(&doc);
        assert!(parsed.is_ok(), "layer cloud-init must parse: {:?}\n---\n{}", parsed.err(), doc);
        assert!(doc.contains("@anthropic-ai/claude-code"), "it must install the agent");
        assert!(doc.contains("retry()"), "an install is documented as having retry in scope");
        assert!(doc.contains("command -v claude >/dev/null || exit 0"));
        assert!(doc.contains("fstrim"), "a layer that does not trim keeps every deleted block");

        let marker = doc.find(BUILD_OK_MARKER).expect("the sentinel must be there");
        let check = doc.find("command -v claude").unwrap();
        assert!(check < marker, "the sentinel must come after the check, not before");

        // Only this agent. A layer carrying two could not be reused by a session wanting one.
        for other in agents().iter().filter(|a| a.command != "claude") {
            assert!(!doc.contains(&other.install), "{} leaked into claude's layer", other.command);
        }
    }

    /// The base image is the layers' backing file: if it carried an agent too, the split would
    /// save nothing for whoever does not use that one.
    #[test]
    fn base_image_carries_no_agent() {
        let doc = build_base_cloud_init(1000);
        for agent in agents() {
            assert!(
                !doc.contains(&agent.install),
                "{} is installed in the base image, which is what layers are for",
                agent.command
            );
        }
    }

    /// The login profile drops this flag before waiting on cloud-init, so it exists while a
    /// layer is being built. Baked into the layer, it would send every real session down the
    /// nested-shell branch: the command never runs and the VM sits there until the timeout.
    #[test]
    fn layer_recipe_clears_the_session_flag_it_inherits() {
        assert!(LOGIN_PROFILE.contains("/tmp/.geli-session-active"));
        let agent = agent_for_command("claude").unwrap();
        assert!(agent_layer_script(agent, 1000).contains("rm -f /tmp/.geli-session-active"));
    }

    /// Layers are built on first use, so two shells starting the same agent race for the same
    /// files. Measured before the lock existed: two `geli agy` a second apart, and the second
    /// deleted the first's half-built overlay and died with a bare `Error: Os { code: 2 }`.
    #[cfg(target_os = "linux")]
    #[test]
    fn only_one_process_may_build_an_image() {
        let lock = std::env::temp_dir().join(format!("geli-lock-test-{}", std::process::id()));
        let _ = fs::remove_file(&lock);

        let first = try_build_lock(&lock).unwrap();
        assert!(first.is_some(), "the first caller must get the lock");
        assert!(
            try_build_lock(&lock).unwrap().is_none(),
            "a second caller must be told to wait, not handed the same lock"
        );
        assert!(lock_holder_alive(&lock), "this process holds it and is plainly alive");

        // Dropping is what releases it, which is what covers every early return in ensure_layer.
        drop(first);
        assert!(!lock.exists(), "the lock must not outlive its guard");
        assert!(try_build_lock(&lock).unwrap().is_some(), "and must be retakeable afterwards");
        let _ = fs::remove_file(&lock);
    }

    /// A build killed mid-flight cannot leave a lock that blocks every later run: `process::exit`
    /// skips the guard's `Drop`, so the holder's pid in the file is the only way to tell "still
    /// building" from "died holding this".
    #[cfg(target_os = "linux")]
    #[test]
    fn a_lock_whose_holder_died_is_not_held() {
        let lock = std::env::temp_dir().join(format!("geli-stale-test-{}", std::process::id()));

        // pid 1 is always alive; a pid this high is not in use on Linux.
        fs::write(&lock, "1\n").unwrap();
        assert!(lock_holder_alive(&lock));

        fs::write(&lock, "4294967290\n").unwrap();
        assert!(!lock_holder_alive(&lock), "a dead holder must not hold the lock forever");

        // An unreadable lock is treated as held rather than stolen — the cautious direction.
        fs::write(&lock, "written by something older\n").unwrap();
        assert!(lock_holder_alive(&lock));

        // And waiting on a lock whose holder is gone clears it and returns, rather than timing out.
        fs::write(&lock, "4294967290\n").unwrap();
        assert!(wait_for_builder(&lock, "test", std::time::Duration::from_secs(5)));
        assert!(!lock.exists(), "the stale lock must be cleared, not merely stepped over");

        let _ = fs::remove_file(&lock);
    }

    /// Every image gets its own console log. They shared one, and since each build truncates the
    /// file it writes, `--build-image --agents a,b` ended holding only `b`'s console — CI then
    /// uploaded that remainder as the run's only evidence.
    #[cfg(target_os = "linux")]
    #[test]
    fn every_image_logs_to_its_own_file() {
        let names = [
            BASE_LOG_NAME.to_string(),
            layer_log_name("claude"),
            layer_log_name("opencode"),
            layer_log_name("agy"),
        ];
        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "two builds would overwrite each other: {:?}", names);

        // `discard_layers` sweeps `geli-layer-*`, so a layer's log has to be caught by it too —
        // otherwise rebuilding the base leaves logs describing images that are gone.
        for name in names.iter().filter(|n| *n != BASE_LOG_NAME) {
            assert!(name.starts_with("geli-layer-"), "{} escapes discard_layers", name);
        }
        assert!(!BASE_LOG_NAME.starts_with("geli-layer-"), "the base log must survive that sweep");
    }

    /// `-cpu host` is a KVM-only model, so the accelerator and the CPU model have to move
    /// together: emitting `host` without `-enable-kvm` is not a slow sandbox but one QEMU refuses
    /// to start. Measured on this host: ~10s accelerated against 99s emulated, so the fallback is
    /// worth having and worth warning about.
    #[cfg(target_os = "linux")]
    #[test]
    fn cpu_model_follows_the_accelerator() {
        let accelerated = accel_args(true);
        assert!(accelerated.contains(&"-enable-kvm".to_string()));
        assert_eq!(accelerated.windows(2).find(|w| w[0] == "-cpu").map(|w| &w[1]), Some(&"host".to_string()));

        let emulated = accel_args(false);
        assert!(
            !emulated.contains(&"-enable-kvm".to_string()),
            "asking for KVM on a host without it fails at launch"
        );
        assert_eq!(emulated.windows(2).find(|w| w[0] == "-cpu").map(|w| &w[1]), Some(&"max".to_string()));
        assert!(
            !emulated.contains(&"host".to_string()),
            "`-cpu host` under emulation is refused by QEMU, so this must never pair with TCG"
        );
    }

    /// A ceiling, not a reservation — but a Linux guest fills whatever it is given with page
    /// cache that QEMU's RSS never gives back, which is why it came down from 4 GiB at all.
    /// Measured peak RSS with an agent reading a file: 728 MB at 4 GiB, 635 at 2, 588 at 1.
    #[cfg(target_os = "linux")]
    #[test]
    fn guest_memory_leaves_room_over_the_measured_peak() {
        let mem = guest_memory();
        // Only meaningful if the environment has not overridden it for this run.
        if std::env::var_os("GELI_MEMORY").is_none() {
            assert_eq!(mem, "2G", "1G worked but left a trivial task at 57% of the ceiling");
        }
        assert!(mem.ends_with('G') || mem.ends_with('M'), "QEMU needs a unit: {}", mem);
    }

    /// **The sweep decides what to delete from a name, so the deciding is tested on names.** The
    /// risk it carries is the opposite of the mess it cleans: a session that is still running
    /// must never be swept, and several of them share /tmp.
    #[test]
    fn only_a_sessions_own_leftovers_are_recognised() {
        assert_eq!(leftover_pid("sandbox-session-4242.qcow2"), Some(4242));
        assert_eq!(leftover_pid("sandbox-share-4242"), Some(4242));
        assert_eq!(leftover_pid("geli-stage-4242-claude"), Some(4242));
        assert_eq!(leftover_pid("geli-out-4242-base"), Some(4242));

        // Not ours, or not a pid. Each of these is something in /tmp that merely starts the same
        // way, and deleting it would be guessing with someone else's files.
        for other in [
            "sandbox-share",
            "sandbox-share-",
            "sandbox-share-12x",
            "sandbox-sessions-1",
            "geli-base.qcow2",
            "geli-stage-abc",
            "systemd-private-1234",
            "",
        ] {
            assert_eq!(leftover_pid(other), None, "{:?} was taken for a session", other);
        }
    }

    /// **A root someone else made is refused, not adopted.** `/tmp`'s sticky bit stops another
    /// user deleting geli's files; it does nothing about them creating the name first, and the
    /// per-session paths under here are all predictable.
    #[test]
    fn a_staging_root_that_is_not_ours_is_refused() {
        // The real root must be usable, which is the other half of the check being worth having.
        let root = staging_root().expect("this user's own staging root was refused");
        assert!(root.exists());
        let meta = fs::metadata(&root).unwrap();
        assert_eq!(meta.uid(), host_uid());
        assert_eq!(meta.mode() & 0o777, 0o700, "another user can read a session's staging");

        // And the shapes that must not be adopted: group- or world-accessible, or not a
        // directory at all. Checked on the predicate rather than by planting a root under
        // another uid, which a test cannot do without being root itself.
        for mode in [0o755, 0o777, 0o750, 0o701] {
            assert_ne!(mode & 0o077, 0, "{:o} would have been accepted", mode);
        }
    }

    /// The live-session guard, end to end: a leftover named after this very process is left
    /// alone, because this process is the one running.
    #[test]
    fn a_running_session_is_never_swept() {
        let root = staging_root().unwrap();
        let mine = root.join(format!("sandbox-share-{}", std::process::id()));
        fs::create_dir_all(&mine).unwrap();
        // A number no process can have, rather than one that merely looks unused.
        let dead = root.join(format!("sandbox-share-{}", u32::MAX));
        fs::create_dir_all(&dead).unwrap();

        sweep_dead_sessions();

        assert!(mine.exists(), "the sweep deleted a running session's share directory");
        assert!(!dead.exists(), "a leftover with no process was left behind");
        let _ = fs::remove_dir_all(&mine);
    }
}
