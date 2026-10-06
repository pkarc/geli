//! The Linux sandbox driver: images on disk, direct kernel boot, and watching the guest come up.

#[allow(unused_imports)]
use crate::dirs_home_dir;
use crate::{guest::*, ui::*};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::os::unix::fs::PermissionsExt;
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
pub(crate) const BUILD_LOG_NAME: &str = "geli-build.log";
pub(crate) const KERNEL_NAME: &str = "geli-vmlinuz";
pub(crate) const INITRD_NAME: &str = "geli-initramfs";
/// What `--build-image` wrote before images were layered. Only used to recognise it and say so.
pub(crate) const LEGACY_IMAGE_NAME: &str = "geli-golden.qcow2";
/// Virtual size of the base image. Alpine's cloud image is 202 MiB virtual, which
/// `apk add nodejs npm` alone overflows. qcow2 is sparse, so this costs nothing until used, and
/// cloud-init's growpart expands the root partition to match on first boot. Agent layers and
/// session overlays inherit this size from their backing file.
pub(crate) const SANDBOX_DISK_SIZE: &str = "20G";

/// Filenames for the layer holding `key` — the chain of agents, in order, joined by `+`.
///
/// The key is the whole cache: `claude` is a layer on the base image, `claude+opencode` a layer
/// on *that*, and a session asking for both reuses the first. Agents are sorted before the key
/// is built, so the same set always names the same chain rather than one per permutation.
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
}

/// Create a private staging directory. The cloud-init payload carries API keys in plaintext,
/// so it must not be world-readable.
pub fn staging_dir(name: &str) -> io::Result<PathBuf> {
    let dir = PathBuf::from(format!("/tmp/{}", name));
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

pub fn run_qemu(
    disk: &Path,
    iso: &Path,
    extra_args: Vec<String>,
    io_mode: QemuIo,
    direct: Option<&DirectBoot>,
) -> io::Result<Child> {
    let mut args: Vec<String> = vec![
        "-m".into(),
        "4G".into(),
        "-enable-kvm".into(),
        // Without this QEMU emulates `qemu64`, a deliberately conservative CPU model missing
        // most modern instruction sets. Passing the host CPU through is both much faster and
        // avoids guest userspace that feature-detects its way into bad paths.
        "-cpu".into(),
        "host".into(),
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
    ];

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
