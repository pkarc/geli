use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, BufRead, Write};
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

    /// Command and arguments passed to execute inside the sandbox
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    agent_args: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct LocalConfig {
    workspace: String,
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

    execute_sandbox(&workspace_name, &current_dir, mapped_dirs, &command_to_run)?;
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

const BASE_IMAGE_NAME: &str = "ubuntu-24.04-server-cloudimg-amd64.img";
const GOLDEN_IMAGE_NAME: &str = "geli-golden.qcow2";
const GOLDEN_RECIPE_NAME: &str = "geli-golden.recipe";
const GOLDEN_BUILD_LOG: &str = "geli-golden-build.log";

/// Printed by the build VM only if every expected tool is actually present. cloud-init does not
/// abort runcmd on failure, so a sentinel that is merely "reached" would prove nothing.
const GOLDEN_OK_MARKER: &str = "GELI_GOLDEN_OK";

/// Baked into the golden image as /home/sandbox/.bash_profile.
///
/// `.bash_profile`, not `.bashrc`: `.bashrc` runs for *every* shell, so a subshell spawned by
/// the agent would re-run the command and poweroff mid-session. Autologin gives a login shell,
/// which is what reads `.bash_profile`.
const GOLDEN_PROFILE: &str = r#"[ -f ~/.bashrc ] && . ~/.bashrc

# Autologin is baked into the image, so the getty can hand us a shell before this session's
# cloud-init has written /etc/geli/session. Wait for it rather than racing it.
cloud-init status --wait >/dev/null 2>&1

[ -f /etc/geli/env ] && . /etc/geli/env

if [ -f /etc/geli/session ]; then
    . /etc/geli/session
    sudo poweroff
else
    echo "[!] geli: no session script found; cloud-init may have failed."
    echo "[!] See /var/log/cloud-init-output.log. Dropping to a shell."
fi
"#;

struct MountPlan {
    /// Body of the shell script that performs every 9p mount inside the guest.
    script: String,
    /// Folder under /workspace the user's command should run in.
    active_folder: String,
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

fn build_mount_script(dirs: &[PathBuf], current_canonical: &Path) -> MountPlan {
    let mut script = String::from("#!/bin/bash\nset -x\n\nmkdir -p /workspace\n");
    let mut active_folder = String::new();

    for (i, dir) in dirs.iter().enumerate() {
        let tag = share_tag(i);
        let name = folder_name(dir);

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

    // If the current directory could not be canonicalized it never matched above; fall back to
    // its name so we never emit a bare `cd /workspace/`.
    if active_folder.is_empty() {
        active_folder = folder_name(current_canonical);
    }

    MountPlan {
        script,
        active_folder,
    }
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

fn build_env_exports(vars: &[(&str, String)]) -> String {
    vars.iter()
        .map(|(key, value)| format!("export {}={}", key, shell_quote(value)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// cloud-config for `--build-image`. Everything slow and workspace-independent lives here.
fn build_golden_cloud_init() -> String {
    format!(
        r#"#cloud-config
users:
  - default
  - name: sandbox
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    lock_passwd: true

package_update: true
packages:
  - nodejs
  - npm
  - python3
  - python3-pip
  - git

write_files:
  - path: /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf
    permissions: '0644'
    content: |
      [Service]
      ExecStart=
      ExecStart=-/sbin/agetty --autologin sandbox --noclear %I $TERM

  - path: /etc/geli/bash_profile
    permissions: '0644'
    content: |
{profile}

runcmd:
  - [npm, install, -g, "@anthropic-ai/claude-code"]
  - install -o sandbox -g sandbox -m 0644 /etc/geli/bash_profile /home/sandbox/.bash_profile
  - apt-get clean
  - rm -rf /var/lib/apt/lists/*
  - bash -c 'command -v claude >/dev/null && command -v git >/dev/null && command -v node >/dev/null && echo {marker}'
  - cloud-init clean --logs --seed
  - poweroff
"#,
        profile = indent_block(GOLDEN_PROFILE, 6),
        marker = GOLDEN_OK_MARKER,
    )
}

/// cloud-config for one sandbox session. Installs nothing: the golden image already has it.
fn build_cloud_init(plan: &MountPlan, command: &str, env_exports: &str) -> String {
    // The echo proves the plumbing worked. Without it, a command that produces no output is
    // indistinguishable from a sandbox that failed to run it at all.
    let session = format!(
        "cd /workspace/{} || true\n\
         echo '[geli] workspace ready; starting your command.'\n\
         {}\n",
        plan.active_folder, command
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

runcmd:
  - bash /etc/geli/mounts.sh
  - chown -R sandbox:sandbox /home/sandbox/.cache /workspace || true
"#,
        env = indent_block(env_exports, 6),
        mounts = indent_block(&plan.script, 6),
        session = indent_block(&session, 6),
    )
}

// --- FULLY IMPLEMENTED LINUX DRIVER ---

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;

    /// Where QEMU's console goes. Sessions inherit the terminal so the agent is interactive;
    /// image builds are unattended and go to a log we can inspect afterwards.
    pub enum QemuIo {
        Interactive,
        LogTo(PathBuf),
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

    pub fn run_qemu(
        disk: &Path,
        iso: &Path,
        extra_args: Vec<String>,
        io_mode: QemuIo,
    ) -> io::Result<()> {
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
            "-device".into(),
            "virtio-net-pci,netdev=net0".into(),
            "-serial".into(),
            "mon:stdio".into(),
        ];
        args.extend(extra_args);

        let mut command = Command::new("qemu-system-x86_64");
        command.args(&args);

        match io_mode {
            // Inherit the terminal so the agent gets a real interactive TTY.
            QemuIo::Interactive => {
                command
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
            }
            QemuIo::LogTo(path) => {
                let log = File::create(&path)?;
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(log.try_clone()?))
                    .stderr(Stdio::from(log));
            }
        }

        let mut child = command.spawn()?;
        child.wait()?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn build_golden_image() -> io::Result<()> {
    use linux::*;

    check_host_tools();

    let dir = images_dir();
    let base_img = dir.join(BASE_IMAGE_NAME);
    if !base_img.exists() {
        eprintln!("Error: Base image not found at {}", base_img.display());
        eprintln!("Run ./setup.sh, or place the Ubuntu cloud image in {}", dir.display());
        std::process::exit(1);
    }

    let recipe = build_golden_cloud_init();
    let pid = std::process::id();
    let stage = staging_dir(&format!("geli-build-{}", pid))?;
    let iso = make_cloud_init_iso(&stage, &recipe, &format!("geli-build-{}", pid), "geli-golden")?;

    // Build into a temporary file and rename on success, so a failed build never leaves a
    // half-provisioned image in place of a working one.
    let target = dir.join(GOLDEN_IMAGE_NAME);
    let pending = dir.join(format!("{}.building", GOLDEN_IMAGE_NAME));
    let log_path = dir.join(GOLDEN_BUILD_LOG);

    create_overlay(&base_img, &pending, Some(SANDBOX_DISK_SIZE))?;

    println!("[*] Building golden image. This takes a few minutes, once.");
    println!("[*] Installing: nodejs, npm, python3, pip, git, @anthropic-ai/claude-code");
    println!("[*] Follow along with:  tail -f {}", log_path.display());

    run_qemu(&pending, &iso, vec![], QemuIo::LogTo(log_path.clone()))?;

    let console = fs::read_to_string(&log_path).unwrap_or_default();
    if !console.contains(GOLDEN_OK_MARKER) {
        let _ = fs::remove_file(&pending);
        eprintln!("\n[!] Golden image build failed: the guest did not report all tools present.");
        eprintln!("    Console log kept at {}", log_path.display());
        std::process::exit(1);
    }

    fs::rename(&pending, &target)?;
    fs::write(dir.join(GOLDEN_RECIPE_NAME), recipe_hash(&recipe))?;
    let _ = fs::remove_dir_all(&stage);

    println!("\n[✓] Golden image ready at {}", target.display());
    println!("    Sandbox sessions now boot from it without installing anything.");
    Ok(())
}

#[cfg(target_os = "linux")]
fn execute_sandbox(ws: &str, cur: &Path, dirs: Vec<PathBuf>, cmd: &str) -> io::Result<()> {
    use linux::*;

    let pid = std::process::id();
    let home = dirs_home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    // Set GELI_KEEP=1 to preserve the session disk and cloud-init files for debugging.
    let keep_session = std::env::var_os("GELI_KEEP").is_some();

    println!("[*] Verifying system hypervisor requirements...");
    check_host_tools();

    let images = images_dir();
    let golden_img = images.join(GOLDEN_IMAGE_NAME);
    if !golden_img.exists() {
        eprintln!("\n[!] Error: Golden image not found at {}", golden_img.display());
        eprintln!("Build it once with:\n  geli --build-image\n");
        std::process::exit(1);
    }

    // Stale images still boot — they are merely out of date, not broken.
    let expected = recipe_hash(&build_golden_cloud_init());
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

    println!("[*] Initializing workspace [{}] with paths:", ws);

    let cur_canon = cur.canonicalize()?;
    let plan = build_mount_script(&dirs, &cur_canon);

    let mut qemu_args: Vec<String> = Vec::new();
    for (i, dir) in dirs.iter().enumerate() {
        let tag = share_tag(i);
        let name = folder_name(dir);

        if is_active_dir(dir, &cur_canon) {
            println!("    -> [ACTIVE] {} => /workspace/{}", dir.display(), name);
        } else {
            println!("    -> [SHARED] {} => /workspace/{}", dir.display(), name);
        }

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

    if let Some(warning) = credential_warning(
        cmd,
        !anthropic_key.trim().is_empty() || !openai_key.trim().is_empty(),
    ) {
        eprintln!("\n{}\n", warning);
    }

    let env_exports = build_env_exports(&[
        ("ANTHROPIC_API_KEY", anthropic_key),
        ("OPENAI_API_KEY", openai_key),
    ]);

    let user_data = build_cloud_init(&plan, cmd, &env_exports);
    let iso = make_cloud_init_iso(
        &vm_share_dir,
        &user_data,
        &format!("geli-{}", pid),
        &format!("sandbox-{}", ws),
    )?;

    // Session overlays sit on top of the golden image and inherit its size.
    create_overlay(&golden_img, &sandbox_img, None)?;

    println!("[*] Launching hardware sandbox via QEMU...");
    run_qemu(&sandbox_img, &iso, qemu_args, QemuIo::Interactive)?;

    if keep_session {
        println!(
            "[*] GELI_KEEP set; preserving {} and {}",
            sandbox_img.display(),
            vm_share_dir.display()
        );
    } else {
        let _ = fs::remove_file(&sandbox_img);
        let _ = fs::remove_dir_all(&vm_share_dir);
        println!("[*] Sandbox wiped cleanly.");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn build_golden_image() -> io::Result<()> {
    eprintln!("Image building is only implemented on Linux.");
    Ok(())
}

#[cfg(target_os = "macos")]
fn execute_sandbox(_ws: &str, _cur: &Path, _dirs: Vec<PathBuf>, _cmd: &str) -> io::Result<()> {
    eprintln!("macOS backend is not yet implemented.");
    Ok(())
}
#[cfg(target_os = "windows")]
fn execute_sandbox(_ws: &str, _cur: &Path, _dirs: Vec<PathBuf>, _cmd: &str) -> io::Result<()> {
    eprintln!("Windows backend is not yet implemented.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use yaml_rust2::YamlLoader;

    fn session_cloud_init(dirs: &[&str], current: &str, cmd: &str) -> String {
        let dirs: Vec<PathBuf> = dirs.iter().map(PathBuf::from).collect();
        let plan = build_mount_script(&dirs, Path::new(current));
        let env = build_env_exports(&[("ANTHROPIC_API_KEY", "sk-test".to_string())]);
        build_cloud_init(&plan, cmd, &env)
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
        let yaml = build_golden_cloud_init();
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
        let yaml = build_golden_cloud_init();
        for expected in ["- git", "- nodejs", "@anthropic-ai/claude-code", "--autologin sandbox"] {
            assert!(yaml.contains(expected), "golden recipe missing {:?}", expected);
        }
        // The marker must be guarded by a real check, not echoed unconditionally.
        assert!(yaml.contains("command -v claude"));
        assert!(yaml.contains(GOLDEN_OK_MARKER));
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
        let plan = build_mount_script(&dirs, &tmp);
        assert_eq!(plan.active_folder, folder_name(&tmp));
    }

    #[test]
    fn build_mount_script_falls_back_when_no_directory_matches() {
        // Nothing canonicalizes to this path, so the fallback in build_mount_script applies.
        let plan = build_mount_script(&[], Path::new("/home/u/myproject"));
        assert_eq!(plan.active_folder, "myproject");
        assert!(!plan.script.contains("cd /workspace/\n"));
    }

    #[test]
    fn recipe_hash_tracks_recipe_changes() {
        let recipe = build_golden_cloud_init();
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

    #[test]
    fn session_announces_itself_before_running_the_command() {
        let yaml = session_cloud_init(&["/home/u/proj"], "/home/u/proj", "claude");
        assert!(yaml.contains("[geli] workspace ready"));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}
