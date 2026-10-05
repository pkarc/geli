//! What the guest is told to be: recipes, cloud-init, mounts, and the boot it reports back.

#[allow(unused_imports)]
use crate::{agents::*, net::*};
use std::process::Command;
use std::path::{Path, PathBuf};


// --- GUEST CONFIGURATION ---
//
// The guest is programmed entirely through cloud-init. Generating that YAML by hand is the
// part that is easy to get wrong, so the documents below have a *fixed* shape: everything
// variable (mount commands, env, the user's command) is injected as a literal block scalar,
// which only requires indenting a block of text uniformly. See `indent_block`.
//
// There are two documents. The golden one is baked once by `--build-image` and holds everything
// static: packages, the agent, autologin, and the login profile. The session one is regenerated
// per run and holds only what depends on the workspace. Keeping the slow parts in the golden
// image is what takes a session boot from minutes to seconds.

pub(crate) const MOUNT_OPTS: &str = "trans=virtio,version=9p2000.L,msize=1048576";

/// Virtual size of the golden image. The Ubuntu cloud image is only 3.5 GiB, which
/// `apt install nodejs npm` alone overflows. qcow2 is sparse, so this costs nothing until used,
/// and cloud-init's growpart expands the root partition to match on first boot. Session
/// overlays inherit this size from their backing file.
pub(crate) const SANDBOX_DISK_SIZE: &str = "20G";

pub(crate) const BASE_IMAGE_NAME: &str = "nocloud_alpine-3.22.2-x86_64-bios-cloudinit-r0.qcow2";
pub(crate) const GOLDEN_IMAGE_NAME: &str = "geli-golden.qcow2";
pub(crate) const GOLDEN_RECIPE_NAME: &str = "geli-golden.recipe";
pub(crate) const GOLDEN_BUILD_LOG: &str = "geli-golden-build.log";

/// Printed by the build VM only if every expected tool is actually present. cloud-init does not
/// abort runcmd on failure, so a sentinel that is merely "reached" would prove nothing.
pub(crate) const GOLDEN_OK_MARKER: &str = "GELI_GOLDEN_OK";

/// Major Node version the agent requires. Alpine's own `nodejs` package satisfies it, so unlike
/// on Ubuntu there is no tarball to fetch — apt's Node 18 was the reason that existed.
pub(crate) const REQUIRED_NODE_MAJOR: u32 = 22;

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

/// Provisioning run once by `--build-image`.
///
/// `bash` is not optional: Alpine's default shell is busybox ash, and the agent's Bash tool
/// needs real bash. Everything else is the same toolchain the Ubuntu recipe installed.
pub(crate) fn golden_setup_script(host_uid: u32) -> String {
    recipe(
        include_str!("guest/setup.sh"),
        &[
            ("@HOST_UID@", &host_uid.to_string()),
            (
                "@AGENT_INSTALLS@",
                &agents().iter().map(|a| a.install.as_str()).collect::<Vec<_>>().join("\n"),
            ),
            ("@MOUNT_OPTS@", MOUNT_OPTS),
            ("@OUT_TAG@", GOLDEN_OUT_TAG),
            ("@KERNEL@", KERNEL_NAME),
            ("@INITRD@", INITRD_NAME),
            ("@META@", GOLDEN_META_NAME),
        ],
    )
}

/// 9p tag the build VM uses to hand the kernel, initramfs and metadata back to the host.
pub(crate) const GOLDEN_OUT_TAG: &str = "geliout";
pub(crate) const KERNEL_NAME: &str = "geli-vmlinuz";
pub(crate) const INITRD_NAME: &str = "geli-initramfs";
pub(crate) const GOLDEN_META_NAME: &str = "geli-golden.meta";

/// Console the kernel and OpenRC write to. The user's terminal is ttyS0; everything the guest
/// says while booting goes here instead, into a log file on the host.
pub(crate) const BOOT_CONSOLE: &str = "ttyS1,115200n8";

/// Phases the host can actually observe, in order, each identified by something the guest itself
/// writes to the boot console. Progress is read from the guest's own output, never guessed from a
/// timer — a progress bar that advances on a clock lies exactly when it matters.
pub(crate) const BOOT_PHASES: &[(&str, &str)] = &[
    ("boot", "OpenRC"),
    ("network", "Starting networking"),
    ("cloud-init", "running 'init'"),
    ("mounts", "mount -t 9p"),
];

/// Written by `mounts.sh` as its last act. The host polls the boot log for it to know the
/// sandbox is ready — `runcmd` output lands in that log, so no extra channel is needed.
pub(crate) const READY_MARKER: &str = "geli:ready";

pub(crate) const GOLDEN_AUTOLOGIN: &str = include_str!("guest/autologin.sh");

/// Printed only when every tool is present *and* Node is new enough. cloud-init does not abort
/// runcmd on failure, so the host greps for this rather than trusting the build "finished".
pub(crate) fn golden_verify_script() -> String {
    recipe(
        include_str!("guest/verify.sh"),
        &[
            ("@NODE_MAJOR@", &REQUIRED_NODE_MAJOR.to_string()),
            ("@OK_MARKER@", GOLDEN_OK_MARKER),
            (
                "@AGENT_BINARIES@",
                &agents()
                    .iter()
                    .map(|a| format!("command -v {} >/dev/null || exit 0", a.binary))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        ],
    )
}

/// Baked into the golden image as /home/sandbox/.bash_profile.
///
/// `.bash_profile`, not `.bashrc`: `.bashrc` runs for *every* shell, so a subshell spawned by
/// the agent would re-run the command and poweroff mid-session.
///
/// That is not enough on its own. Claude Code's Bash tool spawns *login* shells, which read
/// `.bash_profile` too — so every command the agent ran re-entered the session, re-ran the
/// user's command, and hit `sudo poweroff`, killing the VM out from under it. The guard below
/// makes only the boot's first login shell own the session; nested ones just load the
/// environment, so the agent's commands still get TERM and the API keys.
pub(crate) const GOLDEN_PROFILE: &str = include_str!("guest/profile.sh");

pub(crate) struct MountPlan {
    /// Body of the shell script that performs every 9p mount inside the guest.
    pub(crate) script: String,
    /// Folder under /workspace the user's command should run in.
    pub(crate) active_folder: String,
    /// Every folder mounted under /workspace, active one included.
    pub(crate) folders: Vec<String>,
}

/// Prefix every non-empty line with `spaces` spaces. Empty lines are left empty rather than
/// padded, and no trailing newline is emitted — a trailing newline here is what produced the
/// stray empty list entry in the original implementation.
pub(crate) fn indent_block(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    text.lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("{}{}", pad, line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 9p mount tag for the Nth workspace directory. Must match the `-fsdev` id passed to QEMU.
pub(crate) fn share_tag(index: usize) -> String {
    format!("projshare{}", index + 1)
}

pub(crate) fn folder_name(dir: &Path) -> String {
    dir.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

pub(crate) fn is_active_dir(dir: &Path, current_canonical: &Path) -> bool {
    match dir.canonicalize() {
        Ok(canonical) => canonical == current_canonical,
        Err(_) => false,
    }
}

/// Quote a value for safe interpolation into a shell `export`.
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Identifies the recipe a golden image was built from, so a stale image can be spotted. Derived
/// from the recipe text itself rather than a hand-maintained constant, which nobody remembers to
/// bump.
pub(crate) fn recipe_hash(recipe: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    recipe.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub(crate) fn build_mount_script(dirs: &[PathBuf], current_canonical: &Path, lockdown: &str) -> MountPlan {
    let mut script = String::from("#!/bin/bash\nset -x\n\nmkdir -p /workspace\n");
    let mut active_folder = String::new();
    let mut folders = Vec::new();

    for (i, dir) in dirs.iter().enumerate() {
        let tag = share_tag(i);
        let name = folder_name(dir);
        folders.push(name.clone());

        if is_active_dir(dir, current_canonical) {
            active_folder = name.clone();
        }

        script.push_str(&format!(
            "mkdir -p /workspace/{name}\nmount -t 9p -o {MOUNT_OPTS} {tag} /workspace/{name}\n"
        ));
    }

    script.push_str("\nmkdir -p /home/sandbox/.cache/npm /home/sandbox/.cache/pip\n");
    script.push_str(&format!(
        "mount -t 9p -o {MOUNT_OPTS} npmcache /home/sandbox/.cache/npm\n"
    ));
    script.push_str(&format!(
        "mount -t 9p -o {MOUNT_OPTS} pipcache /home/sandbox/.cache/pip\n"
    ));

    // Egress is cut here, after the mounts and before the agent ever runs.
    script.push_str(lockdown);

    // /dev is recreated on every boot, so the session's write access to the boot console has to
    // be granted here rather than baked into the image.
    let console = BOOT_CONSOLE.split(',').next().unwrap_or("ttyS1");
    script.push_str(&format!("\nchmod 0666 /dev/{console} || true\n"));

    // Last line on purpose: this lands in the boot console log, which the host polls to know the
    // sandbox is ready. runcmd output goes there, so no extra channel is needed.
    script.push_str(&format!("echo {READY_MARKER}\n"));

    // If the current directory could not be canonicalized it never matched above; fall back to
    // its name so we never emit a bare `cd /workspace/`.
    if active_folder.is_empty() {
        active_folder = folder_name(current_canonical);
    }

    MountPlan {
        script,
        active_folder,
        folders,
    }
}

/// Claude Code keeps its "already answered that" state in ~/.claude.json. The sandbox is
/// disposable, so without seeding it the agent re-runs onboarding every single session: approve
/// the API key, then trust the folder, every time.
///
/// `approved` holds the last 20 characters of the key rather than the key itself.
pub(crate) fn build_claude_config(folders: &[String], api_key: &str) -> String {
    use serde_json::{json, Map, Value};

    let mut document = Map::new();

    let mut projects = Map::new();
    for folder in folders {
        projects.insert(
            format!("/workspace/{}", folder),
            json!({ "hasTrustDialogAccepted": true }),
        );
    }

    let chars: Vec<char> = api_key.trim().chars().collect();
    let approved: Vec<Value> = if chars.is_empty() {
        Vec::new()
    } else {
        let start = chars.len().saturating_sub(20);
        vec![Value::String(chars[start..].iter().collect())]
    };

    // Inserted last so they always win over anything seeded from the host — `projects` in
    // particular must describe this sandbox's mounts, never the host's project list.
    document.insert("hasCompletedOnboarding".to_string(), json!(true));
    document.insert(
        "customApiKeyResponses".to_string(),
        json!({ "approved": approved, "rejected": [] }),
    );
    document.insert("projects".to_string(), Value::Object(projects));

    serde_json::to_string_pretty(&Value::Object(document)).unwrap_or_else(|_| "{}".to_string())
}

/// The serial console hands the guest a generic `TERM` and a fixed 80x24, regardless of the terminal
/// geli was launched from. A TUI then renders in eight colours in a cramped window. Forwarding
/// the host's terminal identity fixes both.
///
/// Serial lines carry no SIGWINCH, so this is a snapshot: resizing the window mid-session will
/// not propagate.
pub(crate) fn build_terminal_setup(term: &str, colorterm: &str, size: Option<(u16, u16)>) -> String {
    let term = if term.trim().is_empty() {
        "xterm-256color"
    } else {
        term.trim()
    };

    let mut out = format!("export TERM={}", shell_quote(term));

    if !colorterm.trim().is_empty() {
        out.push_str(&format!("\nexport COLORTERM={}", shell_quote(colorterm.trim())));
    }

    if let Some((rows, cols)) = size {
        out.push_str(&format!("\nstty rows {} cols {} 2>/dev/null || true", rows, cols));
    }

    out
}

/// Empty values are skipped rather than exported blank: Claude Code treats a set
/// `ANTHROPIC_API_KEY` as taking precedence over an OAuth login, so exporting an empty one would
/// shadow forwarded credentials.
pub(crate) fn build_env_exports(vars: &[(&str, String)]) -> String {
    vars.iter()
        .filter(|(_, value)| !value.trim().is_empty())
        .map(|(key, value)| format!("export {}={}", key, shell_quote(value)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Optional extra `write_files` entry carrying the host's Claude credentials.
///
/// Copied, not mounted: the sandbox's job is to protect files the agent was not given, and a
/// credential is not one of those. The copy is what makes the agent bill the user's plan instead
/// of API credits.
pub(crate) fn build_credentials_entry(credentials: &[(String, String)]) -> String {
    credentials
        .iter()
        .map(|(path, contents)| {
            format!(
                "  - path: /home/sandbox/{}\n    \
                 permissions: '0600'\n    \
                 owner: sandbox:sandbox\n    \
                 content: |\n{}\n",
                path,
                indent_block(contents, 6)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
