use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const LOCAL_CONFIG_FILE: &str = ".geli.json";

#[derive(Parser, Debug)]
#[command(name = "geli", version = "1.0", about = "Secure Sandbox for AI Agents")]
struct Cli {
    /// Show active isolated namespaces registry profiles
    #[arg(long)]
    list: bool,

    /// Build (or rebuild) the golden image every sandbox session boots from
    #[arg(long)]
    build_image: bool,

    /// Do not copy the host's Claude credentials into the sandbox
    #[arg(long)]
    no_credentials: bool,

    /// Allow the sandbox to reach only an allowlist of hosts, instead of the whole internet
    #[arg(long)]
    restrict_net: bool,

    /// Command and arguments passed to execute inside the sandbox
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    agent_args: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct LocalConfig {
    workspace: String,
    /// Extra hosts this project may reach when `--restrict-net` is on. Defaulted so the
    /// `.geli.json` files that already exist keep loading.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allow: Vec<String>,
}

fn main() -> io::Result<()> {
    let args = Cli::parse();

    if args.list {
        display_active_workspaces()?;
        return Ok(());
    }

    if args.build_image {
        return build_golden_image();
    }

    if args.agent_args.is_empty() {
        eprintln!("Error: No execution command specified.");
        eprintln!("Usage: geli <command> [args...]");
        std::process::exit(1);
    }

    let current_dir = std::env::current_dir()?;
    let workspace_name = get_or_create_workspace(&current_dir)?;
    register_directory_to_workspace(&workspace_name, &current_dir)?;

    let mapped_dirs = get_directories_in_workspace(&workspace_name)?;
    let command_to_run = args.agent_args.join(" ");

    execute_sandbox(
        &workspace_name,
        &current_dir,
        mapped_dirs,
        &command_to_run,
        !args.no_credentials,
        args.restrict_net,
    )?;
    Ok(())
}

fn dirs_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn get_config_dir() -> PathBuf {
    let mut path = dirs_home_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push(".config");
    path.push("geli");
    path.push("workspaces");
    path
}

fn read_local_allowlist(current_dir: &Path) -> Vec<String> {
    fs::read_to_string(current_dir.join(LOCAL_CONFIG_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str::<LocalConfig>(&raw).ok())
        .map(|c| c.allow)
        .unwrap_or_default()
}

fn get_or_create_workspace(current_dir: &Path) -> io::Result<String> {
    let local_config_path = current_dir.join(LOCAL_CONFIG_FILE);

    if local_config_path.exists() {
        let file = File::open(local_config_path)?;
        let config: LocalConfig = serde_json::from_reader(file)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        return Ok(config.workspace);
    }

    println!("[-] No local sandbox configuration found for this directory.");
    println!("[?] How would you like to namespace this folder?");
    println!("------------------------------------------------");

    let config_dir = get_config_dir();
    let mut existing_workspaces = Vec::new();
    if config_dir.exists() {
        for entry in fs::read_dir(&config_dir)? {
            let entry = entry?;
            if let Some(ext) = entry.path().extension() {
                if ext == "txt" {
                    if let Some(name) = entry.path().file_stem().and_then(|s| s.to_str()) {
                        existing_workspaces.push(name.to_string());
                    }
                }
            }
        }
    }

    let mut ws_name = String::new();
    let stdin = io::stdin();

    if !existing_workspaces.is_empty() {
        for (i, ws) in existing_workspaces.iter().enumerate() {
            println!("  {}) Add to existing workspace: [{}]", i + 1, ws);
        }
        println!("  n) Create a BRAND NEW workspace");
        println!("------------------------------------------------");

        print!("Select an option (1-{} or n): ", existing_workspaces.len());
        io::stdout().flush()?;

        let mut choice = String::new();
        stdin.lock().read_line(&mut choice)?;
        let choice = choice.trim();

        if let Ok(num) = choice.parse::<usize>() {
            if num > 0 && num <= existing_workspaces.len() {
                ws_name = existing_workspaces[num - 1].clone();
            }
        }
    } else {
        println!("  (No existing workspaces found. Let's create your first one!)");
        println!("------------------------------------------------");
    }

    // If it's a new workspace (or they typed 'n'), prompt for the name
    if ws_name.is_empty() {
        print!("Enter a name for this workspace: ");
        io::stdout().flush()?;
        let mut new_name = String::new();
        stdin.lock().read_line(&mut new_name)?;
        ws_name = new_name
            .trim()
            .to_lowercase()
            .replace(|c: char| !c.is_alphanumeric() && c != '-', "");
    }

    let config_payload = LocalConfig {
        workspace: ws_name.clone(),
        allow: Vec::new(),
    };
    let local_file = File::create(local_config_path)?;
    serde_json::to_writer_pretty(local_file, &config_payload).map_err(io::Error::other)?;

    Ok(ws_name)
}

fn register_directory_to_workspace(workspace: &str, current_dir: &Path) -> io::Result<()> {
    let config_dir = get_config_dir();
    fs::create_dir_all(&config_dir)?;
    let registry_file = config_dir.join(format!("{}.txt", workspace));

    let path_str = current_dir.to_str().unwrap_or("");
    let mut contents = String::new();
    if registry_file.exists() {
        contents = fs::read_to_string(&registry_file)?;
    }

    if !contents.lines().any(|l| l == path_str) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(registry_file)?;
        writeln!(file, "{}", path_str)?;
    }
    Ok(())
}

fn get_directories_in_workspace(workspace: &str) -> io::Result<Vec<PathBuf>> {
    let registry_file = get_config_dir().join(format!("{}.txt", workspace));
    if !registry_file.exists() {
        return Ok(vec![]);
    }

    let contents = fs::read_to_string(registry_file)?;
    let paths = contents
        .lines()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    Ok(paths)
}

fn display_active_workspaces() -> io::Result<()> {
    let config_dir = get_config_dir();
    if !config_dir.exists() {
        println!("[*] No active sandbox workspaces found.");
        return Ok(());
    }
    println!("=============================================");
    println!("       ACTIVE BINBOX WORKSPACE REGISTRY       ");
    println!("=============================================");
    for entry in fs::read_dir(config_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("txt") {
            let ws_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown");
            println!("Workspace: [{}]", ws_name);
            let contents = fs::read_to_string(&path)?;
            for line in contents.lines() {
                println!("  -> {}", line);
            }
            println!("---------------------------------------------");
        }
    }
    Ok(())
}

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

const MOUNT_OPTS: &str = "trans=virtio,version=9p2000.L,msize=1048576";

/// Virtual size of the golden image. The Ubuntu cloud image is only 3.5 GiB, which
/// `apt install nodejs npm` alone overflows. qcow2 is sparse, so this costs nothing until used,
/// and cloud-init's growpart expands the root partition to match on first boot. Session
/// overlays inherit this size from their backing file.
const SANDBOX_DISK_SIZE: &str = "20G";

const BASE_IMAGE_NAME: &str = "nocloud_alpine-3.22.2-x86_64-bios-cloudinit-r0.qcow2";
const GOLDEN_IMAGE_NAME: &str = "geli-golden.qcow2";
const GOLDEN_RECIPE_NAME: &str = "geli-golden.recipe";
const GOLDEN_BUILD_LOG: &str = "geli-golden-build.log";

/// Printed by the build VM only if every expected tool is actually present. cloud-init does not
/// abort runcmd on failure, so a sentinel that is merely "reached" would prove nothing.
const GOLDEN_OK_MARKER: &str = "GELI_GOLDEN_OK";

/// Major Node version the agent requires. Alpine's own `nodejs` package satisfies it, so unlike
/// on Ubuntu there is no tarball to fetch — apt's Node 18 was the reason that existed.
const REQUIRED_NODE_MAJOR: u32 = 22;

/// The uid the guest's `sandbox` user must take: files arrive over 9p owned by the host user,
/// so a mismatch leaves the agent unable to write to the project it was given.
fn host_uid() -> u32 {
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
fn golden_setup_script(host_uid: u32) -> String {
    format!(
        r#"#!/bin/sh
set -eux

# Roughly a fifth of outbound connections on a given network can time out or drop mid-TLS, and
# `set -e` turns any one of them into a failed build. Every network step gets retries.
retry() {{
  n=0
  until [ "$n" -ge 5 ]; do
    "$@" && return 0
    n=$((n + 1))
    echo "geli: network step failed, retry $n/5: $*"
    sleep 3
  done
  return 1
}}

retry apk update
retry apk add --no-cache bash nodejs npm git python3 py3-pip sudo

# The user is created here, not through cloud-init: Alpine's users module cannot set an explicit
# uid and fails the whole module when asked to. The uid has to match the host's, because files
# arrive over 9p owned by the host user — otherwise the agent can read the project but not write.
deluser alpine 2>/dev/null || true
rm -rf /home/alpine
adduser -D -u {host_uid} -s /bin/bash sandbox
printf 'sandbox ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/sandbox
chmod 0440 /etc/sudoers.d/sandbox

retry npm install -g @anthropic-ai/claude-code

install -o sandbox -g sandbox -m 0644 /etc/geli/bash_profile /home/sandbox/.bash_profile

# Autologin on the serial console. Alpine has no systemd, so this is an inittab line plus a
# login helper rather than a getty drop-in. No getty at all: busybox init already opens ttyS0
# as the controlling tty with sane modes, and `getty -n` unconditionally writes a CRLF, which
# was the stray blank line at the top of every session's stdout.
sed -i 's|^ttyS0::respawn:.*|ttyS0::respawn:/usr/local/bin/geli-autologin|' /etc/inittab

# A disposable VM has no use for a clock daemon or an ssh server, and chronyd alone cost ~4s of
# boot slewing the clock the host already provides.
rc-update del chronyd default || true
rc-update del sshd default || true
rc-update del rdate default || true

# QEMU's user networking always hands out the same addresses, so DHCP is pure latency:
# dhcpcd negotiating a lease was most of this image's boot time.
printf 'auto lo\niface lo inet loopback\n\nauto eth0\niface eth0 inet static\n    address 10.0.2.15\n    netmask 255.255.255.0\n    gateway 10.0.2.2\n' > /etc/network/interfaces
printf 'nameserver 10.0.2.3\n' > /etc/resolv.conf
printf 'network:\n  config: disabled\n' > /etc/cloud/cloud.cfg.d/99-geli-network.cfg

# Sessions boot the kernel directly, so the boot menu never runs — but a hand-booted image
# still shouldn't sit for 10 seconds waiting for a keypress nobody will make.
sed -i 's/^timeout=.*/timeout=1/' /etc/update-extlinux.conf || true
update-extlinux || true
sed -i 's/^TIMEOUT .*/TIMEOUT 1/; s/^PROMPT .*/PROMPT 0/' /boot/extlinux.conf || true

# The login banner and MOTD are the last guest output the user would see on a clean session.
# Removed rather than emptied: busybox login still prints a newline for an empty motd.
rm -f /etc/motd
: > /etc/issue

# Hand the kernel, the initramfs and the cmdline out to the host: sessions boot them directly
# with -kernel/-initrd, which skips SeaBIOS, iPXE and the bootloader entirely. This is the only
# point where we run as root, and those files are 0600 root-only.
mkdir -p /mnt/geli-out
mount -t 9p -o {mount_opts} {out_tag} /mnt/geli-out
cp /boot/vmlinuz-virt /mnt/geli-out/{kernel_name}
cp /boot/initramfs-virt /mnt/geli-out/{initrd_name}

# Captured, never hardcoded: `root=` depends on how the image labels its filesystem.
{{
  printf 'cmdline=%s\n' "$(cat /proc/cmdline)"
  printf 'alpine=%s\n' "$(cut -d' ' -f1-2 /etc/alpine-release 2>/dev/null || echo unknown)"
  printf 'node=%s\n' "$(node --version 2>/dev/null | tr -d v)"
  printf 'claude=%s\n' "$(claude --version 2>/dev/null | cut -d' ' -f1)"
}} > /mnt/geli-out/{meta_name}

chown -R {host_uid}:{host_uid} /mnt/geli-out
sync
umount /mnt/geli-out

rm -rf /var/cache/apk/*
"#,
        mount_opts = MOUNT_OPTS,
        out_tag = GOLDEN_OUT_TAG,
        kernel_name = KERNEL_NAME,
        initrd_name = INITRD_NAME,
        meta_name = GOLDEN_META_NAME,
        host_uid = host_uid,
    )
}

/// 9p tag the build VM uses to hand the kernel, initramfs and metadata back to the host.
const GOLDEN_OUT_TAG: &str = "geliout";
const KERNEL_NAME: &str = "geli-vmlinuz";
const INITRD_NAME: &str = "geli-initramfs";
const GOLDEN_META_NAME: &str = "geli-golden.meta";

/// Console the kernel and OpenRC write to. The user's terminal is ttyS0; everything the guest
/// says while booting goes here instead, into a log file on the host.
const BOOT_CONSOLE: &str = "ttyS1,115200n8";

/// Phases the host can actually observe, in order, each identified by something the guest itself
/// writes to the boot console. Progress is read from the guest's own output, never guessed from a
/// timer — a progress bar that advances on a clock lies exactly when it matters.
const BOOT_PHASES: &[(&str, &str)] = &[
    ("boot", "OpenRC"),
    ("network", "Starting networking"),
    ("cloud-init", "running 'init'"),
    ("mounts", "mount -t 9p"),
];

/// Written by `mounts.sh` as its last act. The host polls the boot log for it to know the
/// sandbox is ready — `runcmd` output lands in that log, so no extra channel is needed.
const READY_MARKER: &str = "geli:ready";

const GOLDEN_AUTOLOGIN: &str = r#"#!/bin/sh
exec /bin/login -f sandbox
"#;

/// Printed only when every tool is present *and* Node is new enough. cloud-init does not abort
/// runcmd on failure, so the host greps for this rather than trusting the build "finished".
fn golden_verify_script() -> String {
    format!(
        r#"#!/bin/sh
command -v claude >/dev/null || exit 0
command -v git >/dev/null || exit 0
command -v node >/dev/null || exit 0

major=$(node -p 'process.versions.node.split(".")[0]')
[ "$major" -ge {required} ] || exit 0

# Without these the host cannot boot the kernel directly, so the image is not publishable.
[ -s /boot/vmlinuz-virt ] || exit 0
[ -s /boot/initramfs-virt ] || exit 0

echo {marker}
"#,
        required = REQUIRED_NODE_MAJOR,
        marker = GOLDEN_OK_MARKER,
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
const GOLDEN_PROFILE: &str = r#"[ -f ~/.bashrc ] && . ~/.bashrc

if [ -z "$GELI_SESSION_ACTIVE" ] && [ ! -e /tmp/.geli-session-active ]; then
    export GELI_SESSION_ACTIVE=1
    : > /tmp/.geli-session-active 2>/dev/null

    # Autologin is baked into the image, so the getty can hand us a shell before this session's
    # cloud-init has written /etc/geli/session. Wait for it rather than racing it.
    cloud-init status --wait >/dev/null 2>&1

    [ -f /etc/geli/env ] && . /etc/geli/env

    if [ -f /etc/geli/session ]; then
        . /etc/geli/session
        # The project lives on 9p, so flush before cutting power. `poweroff -f` skips the wall
        # broadcast and the orderly-shutdown log, neither of which belongs on the user's screen.
        sync
        sudo poweroff -f
    else
        echo "[!] geli: no session script found; cloud-init may have failed."
        echo "[!] See /var/log/cloud-init-output.log. Dropping to a shell."
    fi
else
    # Nested login shell, e.g. an agent's Bash tool. Load the environment, own nothing.
    [ -f /etc/geli/env ] && . /etc/geli/env
fi
"#;

struct MountPlan {
    /// Body of the shell script that performs every 9p mount inside the guest.
    script: String,
    /// Folder under /workspace the user's command should run in.
    active_folder: String,
    /// Every folder mounted under /workspace, active one included.
    folders: Vec<String>,
}

/// Prefix every non-empty line with `spaces` spaces. Empty lines are left empty rather than
/// padded, and no trailing newline is emitted — a trailing newline here is what produced the
/// stray empty list entry in the original implementation.
fn indent_block(text: &str, spaces: usize) -> String {
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
fn share_tag(index: usize) -> String {
    format!("projshare{}", index + 1)
}

fn folder_name(dir: &Path) -> String {
    dir.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

fn is_active_dir(dir: &Path, current_canonical: &Path) -> bool {
    match dir.canonicalize() {
        Ok(canonical) => canonical == current_canonical,
        Err(_) => false,
    }
}

/// Quote a value for safe interpolation into a shell `export`.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Identifies the recipe a golden image was built from, so a stale image can be spotted. Derived
/// from the recipe text itself rather than a hand-maintained constant, which nobody remembers to
/// bump.
fn recipe_hash(recipe: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    recipe.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn build_mount_script(dirs: &[PathBuf], current_canonical: &Path, lockdown: &str) -> MountPlan {
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
fn build_claude_config(folders: &[String], api_key: &str) -> String {
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

/// True if the command's first word is the agent, however it is pathed.
fn command_invokes_claude(command: &str) -> bool {
    command
        .split_whitespace()
        .next()
        .map(|word| word.rsplit('/').next().unwrap_or(word) == "claude")
        .unwrap_or(false)
}

/// Warning shown *before* booting, so a doomed run costs a few seconds rather than a full boot
/// followed by a console that sits there silently.
///
/// geli deliberately does not forward the host's `~/.claude` OAuth credentials into the sandbox,
/// so an agent with no API key reaches its first-run login flow and waits for input that never
/// arrives — which looks exactly like a hang.
fn credential_warning(command: &str, has_credentials: bool) -> Option<String> {
    if has_credentials {
        return None;
    }

    if command_invokes_claude(command) {
        Some(
            "[!] No ANTHROPIC_API_KEY is set.\n\
             \x20   geli does not forward your host's ~/.claude credentials into the sandbox, so\n\
             \x20   `claude` will stop at its first-run login flow and wait for input that never\n\
             \x20   arrives. The session will look like it has hung.\n\
             \x20   Export ANTHROPIC_API_KEY before running geli."
                .to_string(),
        )
    } else {
        Some("[!] No ANTHROPIC_API_KEY or OPENAI_API_KEY set; the sandbox will have no API credentials.".to_string())
    }
}

/// The serial console hands the guest a generic `TERM` and a fixed 80x24, regardless of the terminal
/// geli was launched from. A TUI then renders in eight colours in a cramped window. Forwarding
/// the host's terminal identity fixes both.
///
/// Serial lines carry no SIGWINCH, so this is a snapshot: resizing the window mid-session will
/// not propagate.
fn build_terminal_setup(term: &str, colorterm: &str, size: Option<(u16, u16)>) -> String {
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
fn build_env_exports(vars: &[(&str, String)]) -> String {
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
fn build_credentials_entry(credentials: Option<&str>) -> String {
    match credentials {
        None => String::new(),
        Some(json) => format!(
            "  - path: /home/sandbox/.claude/.credentials.json\n    \
             permissions: '0600'\n    \
             owner: sandbox:sandbox\n    \
             content: |\n{}\n",
            indent_block(json, 6)
        ),
    }
}

// --- EGRESS POLICY ---
//
// With `--restrict-net` the guest gets `restrict=on`, which blocks outbound traffic *and* DNS,
// and a single forwarded port to a proxy running inside geli. The proxy resolves names on the
// host and only connects to destinations on the allowlist.
//
// What this buys: an arbitrary server is no longer reachable from the sandbox. What it does not
// buy: an agent with `github.com` allowed can still push to a gist. The allowlist narrows
// exfiltration, it does not close it — see README.

/// Reachable by default when restricted: the agent's own endpoints plus the registries a coding
/// agent needs to do real work in a repository.
const DEFAULT_ALLOWED_HOSTS: &[&str] = &[
    "api.anthropic.com",
    "platform.claude.com",
    "console.anthropic.com",
    // MCP connectors reach for this; without it they silently fail to authorise.
    "mcp-proxy.anthropic.com",
    "registry.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "github.com",
    "codeload.github.com",
    "crates.io",
    "static.crates.io",
];

/// The only port the proxy will connect to. A proxy that reaches any port on an allowed host is
/// a general-purpose tunnel, not a policy.
const ALLOWED_PORT: u16 = 443;

/// slirp's alias for the host. A guest connection to `10.0.2.2:N` arrives at the host's
/// `127.0.0.1:N`, which is where the proxy listens.
///
/// This is deliberately *not* `restrict=on` plus `guestfwd`. Measured on QEMU 8.2.2: a guestfwd
/// forwards exactly one connection and then times out forever, with or without `restrict`, so a
/// session died after its first request. Egress is cut by removing the guest's default route
/// instead, which leaves only the on-link 10.0.2.0/24 — the proxy — reachable.
const GUEST_PROXY_HOST: &str = "10.0.2.2";

/// Matches a host against the allowlist. A rule starting with `.` or `*.` also matches
/// subdomains, which private registries tend to need.
fn host_allowed(host: &str, allow: &[String]) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    allow.iter().any(|rule| {
        let rule = rule.trim().to_ascii_lowercase();
        let suffix = rule.strip_prefix("*.").or_else(|| rule.strip_prefix('.'));
        match suffix {
            Some(base) => host == base || host.ends_with(&format!(".{}", base)),
            None => host == rule,
        }
    })
}

/// What the proxy was asked to do.
#[derive(Debug, PartialEq)]
enum ProxyRequest {
    Connect { host: String, port: u16 },
    /// Anything else, kept verbatim so a rejection can be explained rather than just dropped.
    Unsupported(String),
}

fn parse_proxy_request(head: &str) -> ProxyRequest {
    let line = head.lines().next().unwrap_or("").trim();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    if !method.eq_ignore_ascii_case("CONNECT") || target.is_empty() {
        return ProxyRequest::Unsupported(line.chars().take(80).collect());
    }

    let (host, port) = match target.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().unwrap_or(0)),
        // A CONNECT without a port is malformed; default to 443 rather than guessing wider.
        None => (target, ALLOWED_PORT),
    };

    ProxyRequest::Connect {
        host: host.trim_matches(|c| c == '[' || c == ']').to_string(),
        port,
    }
}

/// Shell run as root, before the agent starts, that makes the proxy the only way out.
///
/// Two halves, and both are needed. Dropping the default route leaves the internet unreachable
/// by name *and* by IP while the on-link proxy still answers. Narrowing sudo is what makes that
/// stick: the agent is root in the guest by default, and root can simply add the route back.
fn build_lockdown(restricted: bool) -> String {
    if !restricted {
        return String::new();
    }
    "\n# --- restricted egress ---\n\
     ip route del default || true\n\
     printf 'sandbox ALL=(ALL) NOPASSWD: /sbin/poweroff\\n' > /etc/sudoers.d/sandbox\n\
     chmod 0440 /etc/sudoers.d/sandbox\n"
        .to_string()
}

/// Proxy variables for the guest. Lowercase forms too: several tools read only those.
fn build_proxy_env(proxy_port: Option<u16>) -> String {
    let Some(port) = proxy_port else {
        return String::new();
    };
    let url = format!("http://{}:{}", GUEST_PROXY_HOST, port);
    [
        format!("export HTTP_PROXY={}", shell_quote(&url)),
        format!("export HTTPS_PROXY={}", shell_quote(&url)),
        format!("export http_proxy={}", shell_quote(&url)),
        format!("export https_proxy={}", shell_quote(&url)),
        "export NO_PROXY='localhost,127.0.0.1'".to_string(),
        "export no_proxy='localhost,127.0.0.1'".to_string(),
    ]
    .join("\n")
}

/// The allowlist for this session: the built-in defaults plus anything the project asked for.
fn session_allowlist(extra: &[String]) -> Vec<String> {
    let mut all: Vec<String> = DEFAULT_ALLOWED_HOSTS.iter().map(|h| h.to_string()).collect();
    for host in extra {
        let host = host.trim();
        if !host.is_empty() && !all.iter().any(|h| h.eq_ignore_ascii_case(host)) {
            all.push(host.to_string());
        }
    }
    all
}

/// What `--build-image` recorded about the image it produced.
#[derive(Default, Debug, PartialEq)]
struct GoldenMeta {
    /// The kernel command line the image boots itself with, captured rather than invented.
    cmdline: String,
    alpine: String,
    node: String,
    claude: String,
}

fn parse_golden_meta(raw: &str) -> GoldenMeta {
    let mut meta = GoldenMeta::default();
    for line in raw.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_string();
        match key.trim() {
            "cmdline" => meta.cmdline = value,
            "alpine" => meta.alpine = value,
            "node" => meta.node = value,
            "claude" => meta.claude = value,
            _ => {}
        }
    }
    meta
}

/// Furthest phase the boot log shows evidence of.
fn boot_phase(log: &str) -> &'static str {
    BOOT_PHASES
        .iter()
        .rev()
        .find(|(_, marker)| log.contains(marker))
        .map(|(name, _)| *name)
        .unwrap_or("starting")
}

/// One frame of the loading line. Pure so the colour handling is testable: escape codes in a
/// piped log are noise, and `NO_COLOR` exists.
fn render_spinner_line(frame: char, phase: &str, elapsed: f32, color: bool) -> String {
    if color {
        format!(
            "  \x1b[36m{frame}\x1b[0m booting · \x1b[1m{phase}\x1b[0m \x1b[2m· {elapsed:.1}s\x1b[0m"
        )
    } else {
        format!("  {frame} booting · {phase} · {elapsed:.1}s")
    }
}

/// Rewrite the image's own command line for a direct boot: the guest's console moves off the
/// user's terminal, and the kernel stops narrating.
///
/// `root=` and `modules=` are carried over untouched — they describe how this particular image
/// finds its filesystem, and hardcoding them is the kind of guess that breaks silently. The
/// bootloader's own `BOOT_IMAGE=` and `initrd=` are dropped, since there is no bootloader now.
fn boot_cmdline(original: &str) -> String {
    let mut tokens: Vec<String> = original
        .split_whitespace()
        .filter(|t| {
            !t.starts_with("console=")
                && !t.starts_with("BOOT_IMAGE=")
                && !t.starts_with("initrd=")
                && !t.starts_with("loglevel=")
                && *t != "quiet"
        })
        .map(str::to_string)
        .collect();

    tokens.push(format!("console={}", BOOT_CONSOLE));
    tokens.push("quiet".to_string());
    tokens.push("loglevel=0".to_string());
    tokens.join(" ")
}

/// One mounted directory, as the status block shows it.
struct StatusMount {
    host: String,
    guest: String,
    active: bool,
}

/// The facts worth printing before handing the terminal over: what is mounted, which credential
/// the agent will use, and what is inside the image. Everything else about a session is identical
/// every time, and identical output is noise even when geli writes it.
fn render_status(
    workspace: &str,
    mounts: &[StatusMount],
    auth: &str,
    image: &str,
    net: &str,
) -> String {
    let mut out = format!("geli · workspace {}\n", workspace);
    for m in mounts {
        out.push_str(&format!(
            "  mount  {} → {}{}\n",
            m.host,
            m.guest,
            if m.active { "  (active)" } else { "" }
        ));
    }
    out.push_str(&format!("  auth   {}\n", auth));
    out.push_str(&format!("  net    {}\n", net));
    if !image.is_empty() {
        out.push_str(&format!("  image  {}\n", image));
    }
    out
}

fn describe_image(meta: &GoldenMeta) -> String {
    [
        ("alpine", &meta.alpine),
        ("node", &meta.node),
        ("claude", &meta.claude),
    ]
    .iter()
    .filter(|(_, v)| !v.is_empty())
    .map(|(k, v)| format!("{} {}", k, v))
    .collect::<Vec<_>>()
    .join(" · ")
}

/// cloud-config for `--build-image`. Everything slow and workspace-independent lives here.
///
/// Alpine, not Ubuntu: measured at a third of the disk footprint (632 MB against 1.9 GB) with a
/// newer kernel and far fewer packages — which is the point of a sandbox. The differences from
/// the Ubuntu recipe are apk instead of apt, an inittab line instead of a systemd drop-in, and
/// no Node tarball, since Alpine already ships Node 22.
fn build_golden_cloud_init(host_uid: u32) -> String {
    format!(
        r#"#cloud-config
write_files:
  - path: /usr/local/bin/geli-autologin
    permissions: '0755'
    content: |
{autologin}

  - path: /etc/geli/bash_profile
    permissions: '0644'
    content: |
{profile}

  - path: /etc/geli/setup.sh
    permissions: '0755'
    content: |
{setup}

  - path: /etc/geli/verify.sh
    permissions: '0755'
    content: |
{verify}

runcmd:
  - sh /etc/geli/setup.sh
  - sh /etc/geli/verify.sh
  - cloud-init clean --logs --seed
  - poweroff -f
"#,
        autologin = indent_block(GOLDEN_AUTOLOGIN, 6),
        profile = indent_block(GOLDEN_PROFILE, 6),
        setup = indent_block(&golden_setup_script(host_uid), 6),
        verify = indent_block(&golden_verify_script(), 6),
    )
}

/// cloud-config for one sandbox session. Installs nothing: the golden image already has it.
fn build_cloud_init(
    plan: &MountPlan,
    command: &str,
    env_exports: &str,
    claude_config: &str,
    credentials: Option<&str>,
) -> String {
    // The breadcrumb goes to the boot console, not stdout. It exists so a command that produces
    // no output is still distinguishable from a sandbox that never ran it — but the user's stdout
    // belongs to the command alone, and geli's own status block now covers the visible case.
    let session = format!(
        "cd /workspace/{} || true\n\
         echo '[geli] running: {}' > /dev/{} 2>/dev/null || true\n\
         {}\n",
        plan.active_folder,
        plan.active_folder,
        BOOT_CONSOLE.split(',').next().unwrap_or("ttyS1"),
        command
    );

    format!(
        r#"#cloud-config
write_files:
  - path: /etc/geli/env
    permissions: '0600'
    owner: sandbox:sandbox
    content: |
{env}

  - path: /etc/geli/mounts.sh
    permissions: '0755'
    content: |
{mounts}

  - path: /etc/geli/session
    permissions: '0644'
    owner: sandbox:sandbox
    content: |
{session}

  - path: /home/sandbox/.claude.json
    permissions: '0600'
    owner: sandbox:sandbox
    content: |
{claude_config}

{credentials}
runcmd:
  - bash /etc/geli/mounts.sh
  - chown -R sandbox:sandbox /home/sandbox/.claude /home/sandbox/.claude.json || true
  - chown -R sandbox:sandbox /home/sandbox/.cache /workspace || true
"#,
        env = indent_block(env_exports, 6),
        mounts = indent_block(&plan.script, 6),
        session = indent_block(&session, 6),
        claude_config = indent_block(claude_config, 6),
        credentials = build_credentials_entry(credentials),
    )
}

// --- FULLY IMPLEMENTED LINUX DRIVER ---

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::io::Read;
    use std::net::{TcpListener, TcpStream, ToSocketAddrs};
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Child, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

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
            format!("file={},if=virtio", disk.display()),
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

    /// What the proxy decided, for the end-of-session summary.
    #[derive(Default)]
    pub struct ProxyStats {
        pub allowed: usize,
        pub blocked: usize,
        /// Deduplicated so a retry loop against one host does not fill the summary.
        pub blocked_hosts: std::collections::BTreeSet<String>,
    }

    pub struct Proxy {
        pub port: u16,
        pub stats: Arc<Mutex<ProxyStats>>,
    }

    /// Start the egress proxy on an ephemeral localhost port.
    ///
    /// The accept loop runs on a detached thread: it dies with the process, so there is nothing
    /// to shut down or clean up. The guest reaches it only through slirp's single forwarded port.
    pub fn start_proxy(allow: Vec<String>, log_path: PathBuf) -> io::Result<Proxy> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let stats = Arc::new(Mutex::new(ProxyStats::default()));

        let log = Arc::new(Mutex::new(
            fs::OpenOptions::new().create(true).append(true).open(&log_path)?,
        ));
        let allow = Arc::new(allow);
        let thread_stats = stats.clone();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (allow, log, stats) = (allow.clone(), log.clone(), thread_stats.clone());
                std::thread::spawn(move || serve(stream, &allow, &log, &stats));
            }
        });

        Ok(Proxy { port, stats })
    }

    /// Error responses must say `Connection: close`.
    ///
    /// Without it a client keeps the proxy connection in its pool, believing it reusable, then
    /// sends its next CONNECT down a socket this side has already dropped — and waits out its
    /// own timeout. Observed as git taking 300s on the request *after* a rejected one.
    fn refuse(client: &mut TcpStream, status: &str) {
        let _ = client.write_all(
            format!("HTTP/1.1 {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", status)
                .as_bytes(),
        );
        let _ = client.flush();
        let _ = client.shutdown(std::net::Shutdown::Both);
    }

    fn note(log: &Mutex<File>, line: &str) {
        if let Ok(mut f) = log.lock() {
            let _ = writeln!(f, "{}", line);
        }
    }

    fn serve(mut client: TcpStream, allow: &[String], log: &Mutex<File>, stats: &Mutex<ProxyStats>) {
        // A client that opens a connection and then stalls would otherwise hold a thread for the
        // life of the session. The timeout covers reading the request head only; once the tunnel
        // is established it is cleared, because an idle agent session is legitimately quiet.
        let _ = client.set_read_timeout(Some(Duration::from_secs(20)));

        let mut head = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match client.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => head.extend_from_slice(&buf[..n]),
                Err(_) => return,
            }
            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            // A client that never finishes its request head is not getting unbounded memory.
            if head.len() > 16 * 1024 {
                return;
            }
        }

        let request = parse_proxy_request(&String::from_utf8_lossy(&head));
        let (host, port) = match request {
            ProxyRequest::Connect { host, port } => (host, port),
            ProxyRequest::Unsupported(line) => {
                note(log, &format!("rejected non-CONNECT: {}", line));
                refuse(&mut client, "405 Method Not Allowed");
                return;
            }
        };

        if port != ALLOWED_PORT {
            note(log, &format!("blocked {}:{} (only {} is proxied)", host, port, ALLOWED_PORT));
            deny(&mut client, stats, &host);
            return;
        }
        if !host_allowed(&host, allow) {
            note(log, &format!("blocked {}:{}", host, port));
            deny(&mut client, stats, &host);
            return;
        }

        let upstream = match TcpStream::connect_timeout(
            &match (host.as_str(), port).to_socket_addrs().ok().and_then(|mut a| a.next()) {
                Some(addr) => addr,
                None => {
                    note(log, &format!("allowed {}:{} but DNS failed", host, port));
                    refuse(&mut client, "502 Bad Gateway");
                    return;
                }
            },
            Duration::from_secs(15),
        ) {
            Ok(s) => s,
            Err(e) => {
                note(log, &format!("allowed {}:{} but connect failed: {}", host, port, e));
                refuse(&mut client, "502 Bad Gateway");
                return;
            }
        };

        note(log, &format!("allowed {}:{}", host, port));
        if let Ok(mut s) = stats.lock() {
            s.allowed += 1;
        }
        if client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").is_err() {
            return;
        }
        // Clear the head-reading timeout: a live session can sit idle between requests.
        let _ = client.set_read_timeout(None);
        tunnel(client, upstream);
    }

    fn deny(client: &mut TcpStream, stats: &Mutex<ProxyStats>, host: &str) {
        if let Ok(mut s) = stats.lock() {
            s.blocked += 1;
            s.blocked_hosts.insert(host.to_string());
        }
        refuse(client, "403 Forbidden");
    }

    fn tunnel(client: TcpStream, upstream: TcpStream) {
        let Ok(mut client_read) = client.try_clone() else { return };
        let Ok(mut upstream_write) = upstream.try_clone() else { return };
        let mut client_write = client;
        let mut upstream_read = upstream;

        let up = std::thread::spawn(move || {
            let _ = io::copy(&mut client_read, &mut upstream_write);
            let _ = upstream_write.shutdown(std::net::Shutdown::Write);
        });
        let _ = io::copy(&mut upstream_read, &mut client_write);
        let _ = client_write.shutdown(std::net::Shutdown::Write);
        let _ = up.join();
    }

    /// Show which phase the guest is in and return once it reports ready.
    ///
    /// Returns false on timeout. That matters: a guest whose cloud-init broke will never write
    /// the marker, and the terminal has to be handed over regardless rather than spin forever.
    pub fn track_boot(log: &Path, animate: bool, color: bool, timeout: Duration) -> bool {
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
}

#[cfg(target_os = "linux")]
fn build_golden_image() -> io::Result<()> {
    use linux::*;

    check_host_tools();

    let dir = images_dir();
    // GELI_BASE_IMAGE points the build at a different cloud image — for comparing distros, or
    // for anyone who would rather not start from Ubuntu Server. The recipe assumes apt and
    // cloud-init, so Debian-family images work; others would need the recipe changed.
    let base_img = match std::env::var_os("GELI_BASE_IMAGE") {
        Some(path) => PathBuf::from(path),
        None => dir.join(BASE_IMAGE_NAME),
    };
    if !base_img.exists() {
        eprintln!("Error: Base image not found at {}", base_img.display());
        eprintln!("Run ./setup.sh, or place the base cloud image in {}", dir.display());
        std::process::exit(1);
    }

    let host_uid = host_uid();
    let recipe = build_golden_cloud_init(host_uid);
    let pid = std::process::id();
    let stage = staging_dir(&format!("geli-build-{}", pid))?;
    let iso = make_cloud_init_iso(&stage, &recipe, &format!("geli-build-{}", pid), "geli-golden")?;

    // Build into a temporary file and rename on success, so a failed build never leaves a
    // half-provisioned image in place of a working one.
    let target = dir.join(GOLDEN_IMAGE_NAME);
    let pending = dir.join(format!("{}.building", GOLDEN_IMAGE_NAME));
    let log_path = dir.join(GOLDEN_BUILD_LOG);

    // The build VM hands the kernel, initramfs and metadata back through this share.
    let out_dir = staging_dir(&format!("geli-out-{}", pid))?;
    let out_share = vec![
        "-fsdev".to_string(),
        format!(
            "local,path={},id={},security_model=none",
            out_dir.display(),
            GOLDEN_OUT_TAG
        ),
        "-device".to_string(),
        format!("virtio-9p-pci,fsdev={},mount_tag={}", GOLDEN_OUT_TAG, GOLDEN_OUT_TAG),
    ];

    create_overlay(&base_img, &pending, Some(SANDBOX_DISK_SIZE))?;

    println!("[*] Building golden image. This takes a few minutes, once.");
    println!("[*] Installing: bash, Node {REQUIRED_NODE_MAJOR}, python3, pip, git, @anthropic-ai/claude-code");
    println!("[*] Follow along with:  tail -f {}", log_path.display());

    run_qemu(&pending, &iso, out_share, QemuIo::LogTo(log_path.clone()), None)?.wait()?;

    let console = fs::read_to_string(&log_path).unwrap_or_default();
    if !console.contains(GOLDEN_OK_MARKER) {
        let _ = fs::remove_file(&pending);
        eprintln!("\n[!] Golden image build failed: the guest did not report all tools present.");
        eprintln!("    Console log kept at {}", log_path.display());
        std::process::exit(1);
    }

    // Direct boot is useless without these, so a build that did not produce them is a failure
    // even if the guest reported every tool present.
    for name in [KERNEL_NAME, INITRD_NAME, GOLDEN_META_NAME] {
        let produced = out_dir.join(name);
        if !produced.exists() {
            let _ = fs::remove_file(&pending);
            eprintln!("\n[!] Golden image build failed: the guest did not hand back {}.", name);
            eprintln!("    Console log kept at {}", log_path.display());
            std::process::exit(1);
        }
        fs::copy(&produced, dir.join(name))?;
    }

    fs::rename(&pending, &target)?;
    fs::write(dir.join(GOLDEN_RECIPE_NAME), recipe_hash(&recipe))?;
    let _ = fs::remove_dir_all(&stage);
    let _ = fs::remove_dir_all(&out_dir);

    println!("\n[✓] Golden image ready at {}", target.display());
    println!("    Sandbox sessions boot its kernel directly, installing nothing.");
    Ok(())
}

#[cfg(target_os = "linux")]
fn execute_sandbox(
    ws: &str,
    cur: &Path,
    dirs: Vec<PathBuf>,
    cmd: &str,
    forward_credentials: bool,
    restrict_net: bool,
) -> io::Result<()> {
    use linux::*;

    let pid = std::process::id();
    let home = dirs_home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    // Set GELI_KEEP=1 to preserve the session disk and cloud-init files for debugging.
    let keep_session = std::env::var_os("GELI_KEEP").is_some();

    let started = std::time::Instant::now();
    check_host_tools();

    let images = images_dir();
    let golden_img = images.join(GOLDEN_IMAGE_NAME);
    if !golden_img.exists() {
        eprintln!("\n[!] Error: Golden image not found at {}", golden_img.display());
        eprintln!("Build it once with:\n  geli --build-image\n");
        std::process::exit(1);
    }

    let meta_path = images.join(GOLDEN_META_NAME);
    let kernel = images.join(KERNEL_NAME);
    let initrd = images.join(INITRD_NAME);
    for required in [&meta_path, &kernel, &initrd] {
        if !required.exists() {
            eprintln!("\n[!] Error: {} is missing.", required.display());
            eprintln!("Sessions boot the image's kernel directly. Rebuild it once with:");
            eprintln!("  geli --build-image\n");
            std::process::exit(1);
        }
    }
    let meta = parse_golden_meta(&fs::read_to_string(&meta_path).unwrap_or_default());

    // Stale images still boot — they are merely out of date, not broken.
    let expected = recipe_hash(&build_golden_cloud_init(host_uid()));
    let recorded = fs::read_to_string(images.join(GOLDEN_RECIPE_NAME)).unwrap_or_default();
    if recorded.trim() != expected {
        eprintln!("[!] Golden image was built from a different recipe than this binary expects.");
        eprintln!("    Rebuild when convenient:  geli --build-image");
    }

    let host_cache_dir = home.join(".cache").join("geli-sandbox");
    let npm_cache = host_cache_dir.join("npm");
    let pip_cache = host_cache_dir.join("pip");
    fs::create_dir_all(&npm_cache)?;
    fs::create_dir_all(&pip_cache)?;

    let vm_share_dir = staging_dir(&format!("sandbox-share-{}", pid))?;
    let sandbox_img = PathBuf::from(format!("/tmp/sandbox-session-{}.qcow2", pid));

    let cur_canon = cur.canonicalize()?;
    let plan = build_mount_script(&dirs, &cur_canon, &build_lockdown(restrict_net));

    let mut qemu_args: Vec<String> = Vec::new();
    let mut status_mounts: Vec<StatusMount> = Vec::new();
    for (i, dir) in dirs.iter().enumerate() {
        let tag = share_tag(i);
        let name = folder_name(dir);

        status_mounts.push(StatusMount {
            host: dir.display().to_string(),
            guest: format!("/workspace/{}", name),
            active: is_active_dir(dir, &cur_canon),
        });

        qemu_args.push("-fsdev".to_string());
        qemu_args.push(format!(
            "local,path={},id={},security_model=none",
            dir.display(),
            tag
        ));
        qemu_args.push("-device".to_string());
        qemu_args.push(format!("virtio-9p-pci,fsdev={},mount_tag={}", tag, tag));
    }

    qemu_args.extend(vec![
        "-fsdev".to_string(),
        format!("local,path={},id=npmcache,security_model=none", npm_cache.display()),
        "-device".to_string(),
        "virtio-9p-pci,fsdev=npmcache,mount_tag=npmcache".to_string(),
        "-fsdev".to_string(),
        format!("local,path={},id=pipcache,security_model=none", pip_cache.display()),
        "-device".to_string(),
        "virtio-9p-pci,fsdev=pipcache,mount_tag=pipcache".to_string(),
    ]);

    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    let openai_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();

    // Copied in so the agent authenticates as the user and bills their plan rather than API
    // credits. Opt out with --no-credentials.
    let credentials = if forward_credentials {
        let path = home.join(".claude").join(".credentials.json");
        fs::read_to_string(&path).ok()
    } else {
        None
    };

    let has_credentials = !anthropic_key.trim().is_empty()
        || !openai_key.trim().is_empty()
        || credentials.is_some();

    if let Some(warning) = credential_warning(cmd, has_credentials) {
        eprintln!("\n{}\n", warning);
    }

    let api_key_for_config = anthropic_key.clone();
    // The proxy has to exist before the guest boots: slirp forwards a port straight to it.
    let proxy = if restrict_net {
        let allow = session_allowlist(&read_local_allowlist(cur));
        let count = allow.len();
        Some((start_proxy(allow, vm_share_dir.join("net.log"))?, count))
    } else {
        None
    };

    let mut env_exports = build_env_exports(&[
        ("ANTHROPIC_API_KEY", anthropic_key),
        ("OPENAI_API_KEY", openai_key),
    ]);
    let proxy_env = build_proxy_env(proxy.as_ref().map(|(p, _)| p.port));
    if !proxy_env.is_empty() {
        env_exports.push('\n');
        env_exports.push_str(&proxy_env);
    }
    env_exports.push('\n');
    env_exports.push_str(&build_terminal_setup(
        &std::env::var("TERM").unwrap_or_default(),
        &std::env::var("COLORTERM").unwrap_or_default(),
        host_terminal_size(),
    ));

    let claude_config = build_claude_config(&plan.folders, &api_key_for_config);
    let user_data = build_cloud_init(
        &plan,
        cmd,
        &env_exports,
        &claude_config,
        credentials.as_deref(),
    );
    let iso = make_cloud_init_iso(
        &vm_share_dir,
        &user_data,
        &format!("geli-{}", pid),
        &format!("sandbox-{}", ws),
    )?;

    // Session overlays sit on top of the golden image and inherit its size.
    create_overlay(&golden_img, &sandbox_img, None)?;

    let auth = if credentials.is_some() {
        "claude.ai credentials · bills your plan"
    } else if !api_key_for_config.trim().is_empty() {
        "ANTHROPIC_API_KEY · bills API credits"
    } else {
        "none"
    };
    let net = match &proxy {
        Some((_, count)) => format!("restricted · {} domains allowed", count),
        None => "open · unrestricted".to_string(),
    };
    eprint!(
        "{}",
        render_status(ws, &status_mounts, auth, &describe_image(&meta), &net)
    );

    let console_log = vm_share_dir.join("console.log");
    let direct = DirectBoot {
        kernel,
        initrd,
        cmdline: boot_cmdline(&meta.cmdline),
        console_log: console_log.clone(),
    };

    let mut child = run_qemu(
        &sandbox_img,
        &iso,
        qemu_args,
        QemuIo::Interactive,
        Some(&direct),
    )?;

    // The guest writes nothing to this terminal while it boots — its console is on ttyS1 — so
    // the loading line has the screen to itself. It also owns the readiness poll: the phase it
    // displays and the signal to hand over come from the same log.
    let ready = {
        let log = console_log.clone();
        let animate = std::io::stderr().is_terminal();
        let color = animate && std::env::var_os("NO_COLOR").is_none();
        std::thread::spawn(move || {
            track_boot(&log, animate, color, std::time::Duration::from_secs(90))
        })
        .join()
        .unwrap_or(false)
    };

    if !ready {
        eprintln!("[!] The sandbox never reported ready. Console log: {}", console_log.display());
        eprintln!("    Re-run with GELI_KEEP=1 to keep it after exit.");
    }

    let qemu_status = child.wait()?;
    if !qemu_status.success() {
        eprintln!("[!] QEMU exited with {}. Console log: {}", qemu_status, console_log.display());
    }

    if let Some((proxy, _)) = &proxy {
        if let Ok(stats) = proxy.stats.lock() {
            if stats.blocked > 0 {
                let mut hosts: Vec<&str> = stats.blocked_hosts.iter().map(String::as_str).collect();
                hosts.truncate(4);
                eprintln!(
                    "geli · net: {} allowed, {} blocked ({}{})",
                    stats.allowed,
                    stats.blocked,
                    hosts.join(", "),
                    if stats.blocked_hosts.len() > hosts.len() { ", …" } else { "" }
                );
            } else {
                eprintln!("geli · net: {} allowed, none blocked", stats.allowed);
            }
        }
    }

    let elapsed = started.elapsed().as_secs_f32();
    if keep_session {
        eprintln!(
            "geli · done in {:.1}s · kept {} and {}",
            elapsed,
            sandbox_img.display(),
            vm_share_dir.display()
        );
    } else {
        let _ = fs::remove_file(&sandbox_img);
        let _ = fs::remove_dir_all(&vm_share_dir);
        eprintln!("geli · done in {:.1}s · sandbox destroyed", elapsed);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn build_golden_image() -> io::Result<()> {
    eprintln!("Image building is only implemented on Linux.");
    Ok(())
}

#[cfg(target_os = "macos")]
fn execute_sandbox(
    _ws: &str,
    _cur: &Path,
    _dirs: Vec<PathBuf>,
    _cmd: &str,
    _forward_credentials: bool,
    _restrict_net: bool,
) -> io::Result<()> {
    eprintln!("macOS backend is not yet implemented.");
    Ok(())
}
#[cfg(target_os = "windows")]
fn execute_sandbox(
    _ws: &str,
    _cur: &Path,
    _dirs: Vec<PathBuf>,
    _cmd: &str,
    _forward_credentials: bool,
    _restrict_net: bool,
) -> io::Result<()> {
    eprintln!("Windows backend is not yet implemented.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use yaml_rust2::YamlLoader;

    fn session_cloud_init(dirs: &[&str], current: &str, cmd: &str) -> String {
        let dirs: Vec<PathBuf> = dirs.iter().map(PathBuf::from).collect();
        let plan = build_mount_script(&dirs, Path::new(current), "");
        let env = build_env_exports(&[("ANTHROPIC_API_KEY", "sk-test".to_string())]);
        let config = build_claude_config(&plan.folders, "sk-ant-test-key-0123456789");
        build_cloud_init(&plan, cmd, &env, &config, None)
    }

    /// The bug that made geli never work: a YAML document that does not parse.
    #[test]
    fn session_cloud_init_parses() {
        let cases: Vec<(Vec<&str>, &str)> = vec![
            (vec![], "/home/u/proj"),
            (vec!["/home/u/proj"], "/home/u/proj"),
            (vec!["/home/u/proj", "/home/u/other"], "/home/u/proj"),
        ];

        for (dirs, current) in cases {
            let yaml = session_cloud_init(&dirs, current, "echo hello");
            let parsed = YamlLoader::load_from_str(&yaml);
            assert!(
                parsed.is_ok(),
                "cloud-init failed to parse for dirs={:?}: {:?}\n---\n{}",
                dirs,
                parsed.err(),
                yaml
            );
        }
    }

    #[test]
    fn golden_cloud_init_parses() {
        let yaml = build_golden_cloud_init(1000);
        let parsed = YamlLoader::load_from_str(&yaml);
        assert!(parsed.is_ok(), "{:?}\n---\n{}", parsed.err(), yaml);
    }

    /// The whole point of the golden image: a session must not install anything.
    #[test]
    fn session_cloud_init_installs_nothing() {
        let yaml = session_cloud_init(&["/home/u/proj"], "/home/u/proj", "claude");
        assert!(!yaml.contains("apt-get"), "session still runs apt:\n{}", yaml);
        assert!(!yaml.contains("npm install"), "session still runs npm:\n{}", yaml);
        assert!(!yaml.contains("package_update"));
    }

    #[test]
    fn golden_cloud_init_bakes_tooling_and_login() {
        let yaml = build_golden_cloud_init(1000);
        for expected in ["git", "@anthropic-ai/claude-code", "geli-autologin"] {
            assert!(yaml.contains(expected), "golden recipe missing {:?}", expected);
        }
        // The marker must be guarded by a real check, not echoed unconditionally.
        assert!(yaml.contains("command -v claude"));
        assert!(yaml.contains(GOLDEN_OK_MARKER));

        // Alpine ships Node 22, so no tarball — but the version is still checked, because
        // npm installs onto a too-old runtime with only a warning.
        assert!(yaml.contains("apk add"));
        // Every network step must be retried: a single dropped TLS handshake would otherwise
        // fail the whole build under `set -e`.
        assert!(yaml.contains("retry apk update"));
        assert!(yaml.contains("retry apk add"));
        assert!(yaml.contains("retry npm install"));
        assert!(!yaml.contains("apt-get"), "apt leaked into the Alpine recipe");
        assert!(yaml.contains(&format!("-ge {}", REQUIRED_NODE_MAJOR)));
        // bash is not optional: Alpine defaults to busybox ash and the agent's Bash tool needs bash.
        assert!(yaml.contains("bash"), "bash missing from the Alpine recipe");
    }

    #[test]
    fn cloud_init_has_no_empty_runcmd_entries() {
        let yaml = session_cloud_init(&["/home/u/proj", "/home/u/other"], "/home/u/proj", "echo hi");
        for line in yaml.lines() {
            assert_ne!(line.trim(), "-", "stray empty list entry in:\n{}", yaml);
        }
    }

    #[test]
    fn session_cloud_init_mounts_every_directory() {
        let yaml = session_cloud_init(&["/home/u/proj", "/home/u/other"], "/home/u/proj", "echo hi");
        assert!(yaml.contains("projshare1 /workspace/proj"));
        assert!(yaml.contains("projshare2 /workspace/other"));
        assert!(yaml.contains("cd /workspace/proj"));
    }

    /// The command must never land in .bashrc: it runs for every shell, so a nested subshell
    /// would re-run it and poweroff mid-session.
    #[test]
    fn login_profile_waits_for_cloud_init_and_avoids_bashrc() {
        assert!(GOLDEN_PROFILE.contains("cloud-init status --wait"));
        assert!(GOLDEN_PROFILE.contains("/etc/geli/session"));
        // Sourcing ~/.bashrc is fine; appending the command to it is not.
        assert!(!GOLDEN_PROFILE.contains(">> ~/.bashrc"));
    }

    /// Regression: Claude Code's Bash tool spawns login shells, which read .bash_profile. Without
    /// a guard, every command the agent ran re-entered the session and hit `sudo poweroff`,
    /// shutting the VM down mid-task.
    /// Regression: Alpine's default user takes uid 1000, pushing `sandbox` to 1001. Files
    /// arrive over 9p owned by the host user, so the agent could read the project but not
    /// write to it.
    #[test]
    fn golden_user_takes_the_host_uid() {
        let yaml = build_golden_cloud_init(1000);
        assert!(yaml.contains("adduser -D -u 1000"));
        // Alpine's own default user holds uid 1000 and has to go, or sandbox lands on 1001.
        assert!(yaml.contains("deluser alpine"));
        // The stock image waits 10s at a boot menu nobody is there to answer.
        assert!(yaml.contains("TIMEOUT 1"), "boot menu timeout not disabled");

        let other = build_golden_cloud_init(1234);
        assert!(other.contains("adduser -D -u 1234"));
        // A different uid must produce a different image, or stale images go undetected.
        assert_ne!(recipe_hash(&yaml), recipe_hash(&other));
    }

    #[test]
    fn login_profile_runs_the_session_only_once() {
        assert!(GOLDEN_PROFILE.contains("GELI_SESSION_ACTIVE"));
        assert!(GOLDEN_PROFILE.contains("/tmp/.geli-session-active"));

        // The poweroff must sit inside the guard, never at top level.
        let guard = GOLDEN_PROFILE
            .find("GELI_SESSION_ACTIVE")
            .expect("guard missing");
        let poweroff = GOLDEN_PROFILE.find("sudo poweroff").expect("poweroff missing");
        assert!(poweroff > guard, "poweroff runs before the guard");

        // Both branches source the environment, or the agent's commands lose TERM and keys.
        assert_eq!(GOLDEN_PROFILE.matches(". /etc/geli/env").count(), 2);
    }

    #[test]
    fn indent_block_pads_content_but_not_blank_lines() {
        let out = indent_block("a\n\nb", 4);
        assert_eq!(out, "    a\n\n    b");
        assert!(!out.ends_with('\n'), "trailing newline breaks block scalars");
    }

    #[test]
    fn build_mount_script_marks_current_directory_as_active() {
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        let dirs = vec![tmp.clone()];
        let plan = build_mount_script(&dirs, &tmp, "");
        assert_eq!(plan.active_folder, folder_name(&tmp));
    }

    #[test]
    fn build_mount_script_falls_back_when_no_directory_matches() {
        // Nothing canonicalizes to this path, so the fallback in build_mount_script applies.
        let plan = build_mount_script(&[], Path::new("/home/u/myproject"), "");
        assert_eq!(plan.active_folder, "myproject");
        assert!(!plan.script.contains("cd /workspace/\n"));
    }

    #[test]
    fn recipe_hash_tracks_recipe_changes() {
        let host_uid = host_uid();
    let recipe = build_golden_cloud_init(host_uid);
        assert_eq!(recipe_hash(&recipe), recipe_hash(&recipe));
        assert_ne!(recipe_hash(&recipe), recipe_hash(&format!("{}\n# extra", recipe)));
    }

    /// Regression: `geli claude` with no key booted fine and then sat silently in the agent's
    /// first-run login flow, which is indistinguishable from a broken sandbox.
    #[test]
    fn warns_before_booting_an_agent_with_no_credentials() {
        let warning = credential_warning("claude", false).expect("expected a warning");
        assert!(warning.contains("ANTHROPIC_API_KEY"));
        assert!(warning.contains("first-run login"));

        assert!(credential_warning("claude", true).is_none());
        assert!(credential_warning("/usr/local/bin/claude --resume", false)
            .unwrap()
            .contains("first-run login"));

        // Unrelated commands still get a note, but not the agent-specific explanation.
        let generic = credential_warning("ls -la", false).unwrap();
        assert!(!generic.contains("first-run login"));
    }

    #[test]
    fn command_invokes_claude_matches_only_the_agent() {
        assert!(command_invokes_claude("claude"));
        assert!(command_invokes_claude("claude --resume"));
        assert!(command_invokes_claude("/usr/local/bin/claude"));
        assert!(!command_invokes_claude("claudette"));
        assert!(!command_invokes_claude("echo claude"));
        assert!(!command_invokes_claude(""));
    }

    /// The breadcrumb must stay out of stdout: that stream belongs to the user's command.
    #[test]
    fn session_breadcrumb_goes_to_the_boot_console_not_stdout() {
        let yaml = session_cloud_init(&["/home/u/proj"], "/home/u/proj", "claude");
        assert!(yaml.contains("[geli] running:"));
        assert!(yaml.contains("> /dev/ttyS1"), "breadcrumb would land on the user's terminal");
    }

    /// Regression: the guest inherited TERM=vt220 and a fixed 80x24 from the serial console, so
    /// agent TUIs rendered in eight colours in a cramped window.
    #[test]
    fn terminal_setup_forwards_identity_and_size() {
        let setup = build_terminal_setup("xterm-256color", "truecolor", Some((50, 200)));
        assert!(setup.contains("export TERM='xterm-256color'"));
        assert!(setup.contains("export COLORTERM='truecolor'"));
        assert!(setup.contains("stty rows 50 cols 200"));

        // No terminal to copy from (piped, cron): fall back rather than emitting a broken stty.
        let headless = build_terminal_setup("", "", None);
        assert!(headless.contains("export TERM='xterm-256color'"));
        assert!(!headless.contains("stty"));
        assert!(!headless.contains("COLORTERM"));
    }

    #[test]
    fn terminal_setup_survives_cloud_init() {
        let dirs = [PathBuf::from("/home/u/proj")];
        let plan = build_mount_script(&dirs, Path::new("/home/u/proj"), "");
        let env = format!(
            "{}\n{}",
            build_env_exports(&[("ANTHROPIC_API_KEY", "sk-test".to_string())]),
            build_terminal_setup("screen-256color", "truecolor", Some((24, 100)))
        );
        let yaml = build_cloud_init(&plan, "claude", &env, "{}", None);

        assert!(YamlLoader::load_from_str(&yaml).is_ok(), "{}", yaml);
        assert!(yaml.contains("screen-256color"));
        assert!(yaml.contains("stty rows 24 cols 100"));
    }

    /// Regression: the sandbox is disposable, so without seeded state the agent re-ran
    /// onboarding every session — approve the API key, then trust the folder, every time.
    #[test]
    fn claude_config_preapproves_key_and_trusts_every_workspace() {
        let folders = vec!["proj".to_string(), "other".to_string()];
        let config = build_claude_config(&folders, "sk-ant-api03-ABCDEFGHIJKLMNOPQRST");
        let parsed: serde_json::Value = serde_json::from_str(&config).expect("valid JSON");

        assert_eq!(parsed["hasCompletedOnboarding"], true);
        assert_eq!(parsed["projects"]["/workspace/proj"]["hasTrustDialogAccepted"], true);
        assert_eq!(parsed["projects"]["/workspace/other"]["hasTrustDialogAccepted"], true);

        // Only the last 20 characters are recorded, never the whole key.
        let approved = parsed["customApiKeyResponses"]["approved"][0].as_str().unwrap();
        assert_eq!(approved, "ABCDEFGHIJKLMNOPQRST");
        assert_eq!(approved.len(), 20);
        assert!(!config.contains("sk-ant-api03"));
    }

    #[test]
    fn claude_config_without_a_key_approves_nothing() {
        let config = build_claude_config(&["proj".to_string()], "");
        let parsed: serde_json::Value = serde_json::from_str(&config).unwrap();
        assert_eq!(
            parsed["customApiKeyResponses"]["approved"].as_array().unwrap().len(),
            0
        );
        assert_eq!(parsed["hasCompletedOnboarding"], true);
    }

    #[test]
    fn claude_config_survives_cloud_init() {
        let yaml = session_cloud_init(&["/home/u/proj"], "/home/u/proj", "claude");
        assert!(YamlLoader::load_from_str(&yaml).is_ok(), "{}", yaml);
        assert!(yaml.contains("/home/sandbox/.claude.json"));
        assert!(yaml.contains("hasTrustDialogAccepted"));
    }

    /// The guest config holds exactly what suppresses the first-run prompts, and nothing else.
    ///
    /// Verified on a real sandbox: with credentials present but this file absent, the agent still
    /// runs its onboarding. Seeding anything further from the host — machine/user ids, the cached
    /// account profile — was tried and reverted: it changed no prompt and no measurable startup
    /// time, so it only put more of the user's data in the guest.
    #[test]
    fn claude_config_holds_only_what_suppresses_the_prompts() {
        let config = build_claude_config(&["proj".to_string()], "");
        let parsed: serde_json::Value = serde_json::from_str(&config).unwrap();
        let keys: Vec<&String> = parsed.as_object().unwrap().keys().collect();

        assert_eq!(
            keys,
            vec!["customApiKeyResponses", "hasCompletedOnboarding", "projects"],
            "guest config grew beyond the keys that suppress prompts"
        );
    }

    #[test]
    fn forwarded_credentials_land_in_cloud_init() {
        let dirs = [PathBuf::from("/home/u/proj")];
        let plan = build_mount_script(&dirs, Path::new("/home/u/proj"), "");
        let creds = r#"{"claudeAiOauth":{"accessToken":"tok","refreshToken":"ref"}}"#;
        let yaml = build_cloud_init(&plan, "claude", "", "{}", Some(creds));

        assert!(YamlLoader::load_from_str(&yaml).is_ok(), "{}", yaml);
        assert!(yaml.contains("/home/sandbox/.claude/.credentials.json"));
        assert!(yaml.contains("accessToken"));
        // Must not be world-readable inside the guest.
        let block = yaml.split("/home/sandbox/.claude/.credentials.json").nth(1).unwrap();
        assert!(block.contains("permissions: '0600'"));
    }

    #[test]
    fn omitting_credentials_leaves_the_document_valid() {
        let yaml = session_cloud_init(&["/home/u/proj"], "/home/u/proj", "claude");
        assert!(YamlLoader::load_from_str(&yaml).is_ok(), "{}", yaml);
        assert!(!yaml.contains(".credentials.json"));
    }

    /// A blank ANTHROPIC_API_KEY would shadow forwarded OAuth credentials, because Claude Code
    /// treats the variable's presence as taking precedence over a claude.ai login.
    #[test]
    fn empty_env_vars_are_not_exported() {
        let exports = build_env_exports(&[
            ("ANTHROPIC_API_KEY", String::new()),
            ("OPENAI_API_KEY", "   ".to_string()),
        ]);
        assert_eq!(exports, "");

        let exports = build_env_exports(&[
            ("ANTHROPIC_API_KEY", "sk-real".to_string()),
            ("OPENAI_API_KEY", String::new()),
        ]);
        assert_eq!(exports, "export ANTHROPIC_API_KEY='sk-real'");
    }

    #[test]
    fn forwarded_credentials_count_as_credentials() {
        // No API key, but credentials were copied in: nothing to warn about.
        assert!(credential_warning("claude", true).is_none());
    }

    // --- egress policy ---

    /// The proxy must survive being used more than once. A blocked host is enough to exercise
    /// accept → parse → refuse without touching the real network.
    #[cfg(target_os = "linux")]
    #[test]
    fn proxy_serves_more_than_one_connection() {
        use std::io::{Read, Write};

        let log = std::env::temp_dir().join(format!("geli-proxy-{}.log", std::process::id()));
        let proxy = linux::start_proxy(vec!["allowed.example".to_string()], log.clone())
            .expect("proxy failed to start");

        for attempt in 1..=3 {
            let mut c = std::net::TcpStream::connect(("127.0.0.1", proxy.port))
                .unwrap_or_else(|e| panic!("connection {} refused: {}", attempt, e));
            c.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            c.write_all(b"CONNECT blocked.example:443 HTTP/1.1\r\n\r\n").unwrap();

            let mut buf = [0u8; 64];
            let n = c.read(&mut buf).unwrap_or(0);
            let reply = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(reply.contains("403"), "connection {} got {:?}", attempt, reply);
        }

        let _ = std::fs::remove_file(&log);
    }

    fn allow() -> Vec<String> {
        session_allowlist(&["*.internal.example".to_string(), ".corp.test".to_string()])
    }

    #[test]
    fn allowlist_matches_exactly_and_by_suffix() {
        let a = allow();
        assert!(host_allowed("api.anthropic.com", &a));
        assert!(host_allowed("API.Anthropic.COM", &a), "hosts are case-insensitive");
        assert!(host_allowed("api.anthropic.com.", &a), "a trailing dot is the same host");

        // Suffix rules cover the base and its subdomains, and nothing else.
        assert!(host_allowed("registry.internal.example", &a));
        assert!(host_allowed("internal.example", &a));
        assert!(host_allowed("deep.nested.corp.test", &a));
    }

    /// The failure that matters: something not on the list must not slip through. A suffix rule
    /// must not match a host that merely *ends with the same text*.
    #[test]
    fn allowlist_rejects_everything_else() {
        let a = allow();
        for host in [
            "example.com",
            "evil.com",
            "",
            "anthropic.com",                 // the rule is api.anthropic.com, not the apex
            "api.anthropic.com.evil.com",    // suffix-appending attack
            "notinternal.example",           // `.internal.example` must need the dot
            "fake-corp.test",
        ] {
            assert!(!host_allowed(host, &a), "{:?} should not be allowed", host);
        }
    }

    #[test]
    fn proxy_only_accepts_connect() {
        assert_eq!(
            parse_proxy_request("CONNECT api.anthropic.com:443 HTTP/1.1\r\nHost: x\r\n\r\n"),
            ProxyRequest::Connect { host: "api.anthropic.com".into(), port: 443 }
        );
        // busybox wget proxies plain HTTP as an absolute-form GET; it must be refused, visibly.
        match parse_proxy_request("GET http://example.com/ HTTP/1.1\r\n\r\n") {
            ProxyRequest::Unsupported(line) => assert!(line.contains("GET")),
            other => panic!("absolute-form GET was accepted: {:?}", other),
        }
        assert!(matches!(parse_proxy_request(""), ProxyRequest::Unsupported(_)));
        assert!(matches!(parse_proxy_request("CONNECT\r\n"), ProxyRequest::Unsupported(_)));
    }

    /// A port other than 443 turns the proxy into a general tunnel, so the parse must surface it
    /// for the caller to reject.
    #[test]
    fn proxy_surfaces_the_requested_port() {
        assert_eq!(
            parse_proxy_request("CONNECT github.com:22 HTTP/1.1\r\n\r\n"),
            ProxyRequest::Connect { host: "github.com".into(), port: 22 }
        );
        assert_ne!(22, ALLOWED_PORT);
    }

    #[test]
    fn proxy_env_points_at_the_host_gateway() {
        assert_eq!(build_proxy_env(None), "");
        let env = build_proxy_env(Some(45678));
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "NO_PROXY"] {
            assert!(env.contains(key), "{} missing: some tools read only one form", key);
        }
        // Measured: a guestfwd forwards one connection and then dies, so the proxy is reached
        // through slirp's host alias instead.
        assert!(env.contains("10.0.2.2:45678"));
        assert!(!env.contains("10.0.2.100"));
    }

    /// Both halves of the lockdown are load-bearing. Dropping the route cuts egress; narrowing
    /// sudo is what stops the agent — root in the guest by default — from adding it back.
    #[test]
    fn lockdown_cuts_the_route_and_takes_away_root() {
        assert_eq!(build_lockdown(false), "", "an open session must not be touched");

        let locked = build_lockdown(true);
        assert!(locked.contains("ip route del default"));
        assert!(locked.contains("NOPASSWD: /sbin/poweroff"));
        assert!(
            !locked.contains("NOPASSWD: ALL"),
            "leaving blanket sudo would let the agent undo the route"
        );
    }

    /// The lockdown has to be in place before the agent runs, and the readiness marker is what
    /// tells the host the guest is ready — so the lockdown must come first.
    #[test]
    fn lockdown_runs_before_the_session_is_declared_ready() {
        let plan = build_mount_script(
            &[PathBuf::from("/home/u/proj")],
            Path::new("/home/u/proj"),
            &build_lockdown(true),
        );
        let route = plan.script.find("ip route del default").expect("no lockdown");
        let ready = plan.script.find(READY_MARKER).expect("no ready marker");
        assert!(route < ready, "the agent could run before egress was cut");
    }

    #[test]
    fn session_allowlist_adds_without_duplicating() {
        let base = session_allowlist(&[]);
        assert!(base.contains(&"api.anthropic.com".to_string()));

        let extended = session_allowlist(&[
            "API.ANTHROPIC.COM".to_string(),
            "extra.example".to_string(),
            "   ".to_string(),
        ]);
        assert_eq!(extended.len(), base.len() + 1, "case-folded duplicate or blank crept in");
        assert!(extended.contains(&"extra.example".to_string()));
    }

    /// Adding `allow` must not break the .geli.json files that already exist on disk.
    #[test]
    fn local_config_without_allow_still_parses() {
        let old: LocalConfig = serde_json::from_str(r#"{"workspace":"github"}"#).unwrap();
        assert_eq!(old.workspace, "github");
        assert!(old.allow.is_empty());

        let new: LocalConfig =
            serde_json::from_str(r#"{"workspace":"w","allow":["a.test"]}"#).unwrap();
        assert_eq!(new.allow, vec!["a.test".to_string()]);

        // An empty allowlist must not be written back into users' files.
        let written = serde_json::to_string(&LocalConfig {
            workspace: "w".into(),
            allow: Vec::new(),
        })
        .unwrap();
        assert!(!written.contains("allow"));
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

    #[test]
    fn spinner_line_honours_no_color() {
        let plain = render_spinner_line('⠹', "cloud-init", 2.44, false);
        assert_eq!(plain, "  ⠹ booting · cloud-init · 2.4s");
        assert!(!plain.contains('\x1b'), "escape codes in a piped log are noise");

        let colored = render_spinner_line('⠹', "mounts", 7.0, true);
        assert!(colored.contains('\x1b'));
        assert!(colored.contains("mounts"));
        assert!(colored.contains("7.0s"));
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
    fn golden_meta_round_trips() {
        let meta = parse_golden_meta(
            "cmdline=root=LABEL=/ console=ttyS0\nalpine=3.22.2\nnode=22.23.2\nclaude=2.1.289\n",
        );
        assert_eq!(meta.alpine, "3.22.2");
        assert_eq!(meta.node, "22.23.2");
        assert_eq!(meta.claude, "2.1.289");
        assert!(meta.cmdline.starts_with("root=LABEL=/"));
        assert_eq!(describe_image(&meta), "alpine 3.22.2 · node 22.23.2 · claude 2.1.289");
    }

    #[test]
    fn golden_meta_tolerates_a_missing_or_partial_file() {
        assert_eq!(parse_golden_meta(""), GoldenMeta::default());
        let partial = parse_golden_meta("node=22.23.2\ngarbage line\n");
        assert_eq!(partial.node, "22.23.2");
        // An image line with holes in it should not print empty fields.
        assert_eq!(describe_image(&partial), "node 22.23.2");
    }

    #[test]
    fn status_block_shows_mounts_auth_and_image() {
        let mounts = vec![
            StatusMount { host: "/home/u/proj".into(), guest: "/workspace/proj".into(), active: true },
            StatusMount { host: "/home/u/api".into(), guest: "/workspace/api".into(), active: false },
        ];
        let out = render_status("acme", &mounts, "claude.ai credentials", "alpine 3.22", "open");

        assert!(out.starts_with("geli · workspace acme\n"));
        assert!(out.contains("/home/u/proj → /workspace/proj  (active)"));
        assert!(out.contains("/home/u/api → /workspace/api\n"));
        assert!(out.contains("auth   claude.ai credentials"));
        assert!(out.contains("image  alpine 3.22"));
        // The network posture is a security property that changes per run: always shown.
        assert!(out.contains("net    open"));
    }

    /// Regression guard for the point of this change: the session must leave the guest's console
    /// somewhere other than the terminal, and signal readiness through it.
    #[test]
    fn mount_script_ends_with_the_ready_marker() {
        let plan = build_mount_script(&[PathBuf::from("/home/u/proj")], Path::new("/home/u/proj"), "");
        assert_eq!(
            plan.script.trim_end().lines().last().unwrap().trim(),
            format!("echo {}", READY_MARKER)
        );
        // The session writes its breadcrumb there, and /dev is rebuilt every boot.
        assert!(plan.script.contains("chmod 0666 /dev/ttyS1"));
    }

    #[test]
    fn golden_recipe_silences_the_guest_and_hands_out_the_kernel() {
        let yaml = build_golden_cloud_init(1000);
        // Nothing of the distro's own chatter should reach a clean session.
        assert!(yaml.contains("rm -f /etc/motd"));
        assert!(yaml.contains("/etc/issue"));
        // `getty -n` writes a CRLF before handing over, a blank line on the user's stdout.
        assert!(yaml.contains("ttyS0::respawn:/usr/local/bin/geli-autologin"));
        assert!(!yaml.contains("/sbin/getty"), "autologin went back through getty");
        // Direct boot needs these out of the image.
        assert!(yaml.contains(KERNEL_NAME));
        assert!(yaml.contains(INITRD_NAME));
        assert!(yaml.contains(GOLDEN_META_NAME));
        assert!(yaml.contains("/proc/cmdline"), "the cmdline must be captured, not invented");
    }

    #[test]
    fn login_profile_flushes_before_cutting_power() {
        // The project lives on 9p; a forced poweroff without sync can lose writes.
        // Match the commands, not the comment that mentions them.
        let sync = GOLDEN_PROFILE.find("\n        sync\n").expect("no sync before poweroff");
        let off = GOLDEN_PROFILE.find("sudo poweroff -f").expect("not a forced poweroff");
        assert!(sync < off, "sync must run before power is cut");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}
