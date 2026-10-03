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
    serde_json::to_writer_pretty(local_file, &config_payload)
        .map_err(io::Error::other)?;

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
// part that is easy to get wrong, so the document below has a *fixed* shape: everything
// variable (mount commands, env, the user's command) is injected as a literal block scalar,
// which only requires indenting a block of text uniformly. See `indent_block`.

const MOUNT_OPTS: &str = "trans=virtio,version=9p2000.L,msize=1048576";

/// Virtual size of the per-session overlay. The Ubuntu cloud image is only 3.5 GiB, which
/// `apt install nodejs npm` alone overflows. qcow2 is sparse, so this costs nothing until used,
/// and cloud-init's growpart expands the root partition to match on first boot.
const SANDBOX_DISK_SIZE: &str = "20G";

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

    // Package caches are mounted here, i.e. before anything in runcmd installs packages.
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

fn build_env_exports(vars: &[(&str, String)]) -> String {
    vars.iter()
        .map(|(key, value)| format!("export {}={}", key, shell_quote(value)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_cloud_init(plan: &MountPlan, command: &str, env_exports: &str) -> String {
    // The command lives in .bash_profile, not .bashrc: .bashrc runs for *every* shell, so a
    // subshell spawned by the agent would re-run the command and poweroff mid-session.
    let profile = format!(
        "[ -f ~/.bashrc ] && . ~/.bashrc\n\
         [ -f /etc/geli/env ] && . /etc/geli/env\n\
         cd /workspace/{} || true\n\
         {}\n\
         sudo poweroff\n",
        plan.active_folder, command
    );

    format!(
        r#"#cloud-config
users:
  - default
  - name: sandbox
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    lock_passwd: true

write_files:
  - path: /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf
    permissions: '0644'
    content: |
      [Service]
      ExecStart=
      ExecStart=-/sbin/agetty --autologin sandbox --noclear %I $TERM

  - path: /etc/geli/env
    permissions: '0600'
    content: |
{env}

  - path: /etc/geli/mounts.sh
    permissions: '0755'
    content: |
{mounts}

  - path: /etc/geli/profile
    permissions: '0644'
    content: |
{profile}

runcmd:
  - bash /etc/geli/mounts.sh
  - [apt-get, update]
  - [apt-get, install, -y, nodejs, npm, python3, python3-pip]
  - [npm, install, -g, "@anthropic-ai/claude-code"]
  - install -o sandbox -g sandbox -m 0644 /etc/geli/profile /home/sandbox/.bash_profile
  - chown sandbox:sandbox /etc/geli/env || true
  - chown -R sandbox:sandbox /home/sandbox/.cache /workspace || true
  - systemctl daemon-reload
  - systemctl restart serial-getty@ttyS0.service
"#,
        env = indent_block(env_exports, 6),
        mounts = indent_block(&plan.script, 6),
        profile = indent_block(&profile, 6),
    )
}

// --- FULLY IMPLEMENTED LINUX DRIVER ---

#[cfg(target_os = "linux")]
fn execute_sandbox(ws: &str, cur: &Path, dirs: Vec<PathBuf>, cmd: &str) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let pid = std::process::id();
    let home = dirs_home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    // Set GELI_KEEP=1 to preserve the session disk and cloud-init files for debugging.
    let keep_session = std::env::var_os("GELI_KEEP").is_some();

    println!("[*] Verifying system hypervisor requirements...");

    // Check for qemu-system-x86_64
    let qemu_check = Command::new("which")
        .arg("qemu-system-x86_64")
        .output();

    // Check for genisoimage (required to package configuration drives)
    let geniso_check = Command::new("which")
        .arg("genisoimage")
        .output();

    let qemu_installed = qemu_check.map(|o| o.status.success()).unwrap_or(false);
    let geniso_installed = geniso_check.map(|o| o.status.success()).unwrap_or(false);

    if !qemu_installed || !geniso_installed {
        eprintln!("\n[!] Error: Missing required system virtualization utilities.");
        eprintln!("geli requires QEMU and genisoimage to create secure hardware sandboxes.");
        eprintln!("\nPlease install them by running:");
        eprintln!("  sudo apt update && sudo apt install -y qemu-system-x86 qemu-utils genisoimage\n");
        std::process::exit(1);
    }

    let base_img = home.join("qemu-sandbox").join("ubuntu-24.04-server-cloudimg-amd64.img");
    if !base_img.exists() {
        eprintln!(
            "Error: Base image not found at {}",
            base_img.display()
        );
        eprintln!("Please place the Ubuntu cloud image in ~/qemu-sandbox/");
        std::process::exit(1);
    }

    let host_cache_dir = home.join(".cache").join("geli-sandbox");
    let npm_cache = host_cache_dir.join("npm");
    let pip_cache = host_cache_dir.join("pip");
    fs::create_dir_all(&npm_cache)?;
    fs::create_dir_all(&pip_cache)?;

    let vm_share_dir = PathBuf::from(format!("/tmp/sandbox-share-{}", pid));
    fs::create_dir_all(&vm_share_dir)?;
    // The cloud-init payload carries API keys in plaintext; keep it out of other users' reach.
    fs::set_permissions(&vm_share_dir, fs::Permissions::from_mode(0o700))?;
    let sandbox_img = format!("/tmp/sandbox-session-{}.qcow2", pid);

    println!("[*] Initializing workspace [{}] with paths:", ws);

    let mut qemu_args = vec![
        "-m".to_string(), "4G".to_string(),
        "-enable-kvm".to_string(),
        "-smp".to_string(), "2".to_string(),
        "-nographic".to_string(),
    ];

    let cur_canon = cur.canonicalize()?;
    let plan = build_mount_script(&dirs, &cur_canon);

    for (i, dir) in dirs.iter().enumerate() {
        let tag = share_tag(i);
        let name = folder_name(dir);

        if is_active_dir(dir, &cur_canon) {
            println!("    -> [ACTIVE] {} => /workspace/{}", dir.display(), name);
        } else {
            println!("    -> [SHARED] {} => /workspace/{}", dir.display(), name);
        }

        qemu_args.push("-fsdev".to_string());
        qemu_args.push(format!("local,path={},id={},security_model=none", dir.display(), tag));
        qemu_args.push("-device".to_string());
        qemu_args.push(format!("virtio-9p-pci,fsdev={},mount_tag={}", tag, tag));
    }

    let env_exports = build_env_exports(&[
        ("ANTHROPIC_API_KEY", std::env::var("ANTHROPIC_API_KEY").unwrap_or_default()),
        ("OPENAI_API_KEY", std::env::var("OPENAI_API_KEY").unwrap_or_default()),
    ]);

    let user_data = build_cloud_init(&plan, cmd, &env_exports);

    let user_data_path = vm_share_dir.join("user-data");
    let meta_data_path = vm_share_dir.join("meta-data");
    let iso_path = vm_share_dir.join("cloud-init.iso");
    fs::write(&user_data_path, user_data)?;
    fs::write(&meta_data_path, format!("instance-id: geli-{}\nlocal-hostname: sandbox-{}\n", pid, ws))?;
    // Create cloud-init ISO
    let geniso_status = Command::new("genisoimage")
    .args([
        "-output", iso_path.to_str().unwrap(),
        "-volid", "cidata",
        "-joliet", "-rock",
        user_data_path.to_str().unwrap(),
        meta_data_path.to_str().unwrap(),
    ])
    .output()?;
        if !geniso_status.status.success() {
            eprintln!("Failed to generate cloud-init ISO. Ensure genisoimage is installed.");
            return Ok(());
        }
    // Create disposable QCOW2 snapshot
    let qemu_img_status = Command::new("qemu-img")
    .args([
        "create", "-f", "qcow2", "-F", "qcow2",
        "-b", base_img.to_str().unwrap(),
        &sandbox_img,
        SANDBOX_DISK_SIZE,
    ])
    .output()?;
        if !qemu_img_status.status.success() {
            eprintln!("Failed to create qcow2 snapshot layer.");
            eprintln!("{}", String::from_utf8_lossy(&qemu_img_status.stderr));
            return Ok(());
        }
    // Append standard drives, caches, and networking devices
    qemu_args.extend(vec![
        "-drive".to_string(), format!("file={},if=virtio", sandbox_img),
        "-drive".to_string(), format!("file={},format=raw,if=virtio", iso_path.display()),
        "-fsdev".to_string(), format!("local,path={},id=npmcache,security_model=none", npm_cache.display()),
        "-device".to_string(), "virtio-9p-pci,fsdev=npmcache,mount_tag=npmcache".to_string(),
        "-fsdev".to_string(), format!("local,path={},id=pipcache,security_model=none", pip_cache.display()),
        "-device".to_string(), "virtio-9p-pci,fsdev=pipcache,mount_tag=pipcache".to_string(),
        "-netdev".to_string(), "user,id=net0".to_string(),
        "-device".to_string(), "virtio-net-pci,netdev=net0".to_string(),
        "-serial".to_string(), "mon:stdio".to_string(),
    ]);
    println!("[*] Launching hardware sandbox via QEMU...");
    // Spawn QEMU inheriting the current terminal stdio for full TTY interactivity
        let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
    .stderr(std::process::Stdio::inherit())
        .spawn()?;
        let _ = child.wait();
        // Clean up temporary files on exit
        if keep_session {
            println!("[*] GELI_KEEP set; preserving {} and {}", sandbox_img, vm_share_dir.display());
        } else {
            let _ = fs::remove_file(&sandbox_img);
            let _ = fs::remove_dir_all(&vm_share_dir);
            println!("[*] Sandbox wiped cleanly.");
        }
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

    fn cloud_init_for(dirs: &[&str], current: &str, cmd: &str) -> String {
        let dirs: Vec<PathBuf> = dirs.iter().map(PathBuf::from).collect();
        let plan = build_mount_script(&dirs, Path::new(current));
        let env = build_env_exports(&[("ANTHROPIC_API_KEY", "sk-test".to_string())]);
        build_cloud_init(&plan, cmd, &env)
    }

    /// The bug that made geli never work: a YAML document that does not parse.
    #[test]
    fn cloud_init_parses() {
        let cases: Vec<(Vec<&str>, &str)> = vec![
            (vec![], "/home/u/proj"),
            (vec!["/home/u/proj"], "/home/u/proj"),
            (vec!["/home/u/proj", "/home/u/other"], "/home/u/proj"),
        ];

        for (dirs, current) in cases {
            let yaml = cloud_init_for(&dirs, current, "echo hello");
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
    fn cloud_init_has_no_empty_runcmd_entries() {
        let yaml = cloud_init_for(&["/home/u/proj", "/home/u/other"], "/home/u/proj", "echo hi");
        for line in yaml.lines() {
            assert_ne!(line.trim(), "-", "stray empty list entry in:\n{}", yaml);
        }
    }

    #[test]
    fn cloud_init_mounts_every_directory_and_enables_autologin() {
        let yaml = cloud_init_for(&["/home/u/proj", "/home/u/other"], "/home/u/proj", "echo hi");
        assert!(yaml.contains("projshare1 /workspace/proj"));
        assert!(yaml.contains("projshare2 /workspace/other"));
        assert!(yaml.contains("--autologin sandbox"));
        // The command must land in .bash_profile, never .bashrc (see build_cloud_init).
        assert!(yaml.contains("/home/sandbox/.bash_profile"));
        assert!(!yaml.contains(".bashrc\n            export"));
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
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}
