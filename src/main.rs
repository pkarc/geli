use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

const LOCAL_CONFIG_FILE: &str = ".geli.json";

#[derive(Parser, Debug)]
#[command(name = "geli", version = "1.0", about = "Secure Sandbox for AI Agents")]
struct Cli {
    /// Show active isolated namespaces registry profiles
    #[arg(long)]
    list: bool,

    /// Build (or rebuild) the base image every sandbox session boots from
    #[arg(long)]
    build_image: bool,

    /// With --build-image: also build these agents' layers now, instead of on first use
    #[arg(long, value_delimiter = ',', value_name = "claude,opencode")]
    agents: Vec<String>,

    /// Do not copy the host's credentials into the sandbox
    #[arg(long)]
    no_credentials: bool,

    /// Forward host SSH keys into the sandbox for git operations
    #[arg(long)]
    ssh: bool,

    /// Forward a specific SSH private key into the sandbox
    #[arg(long, value_name = "PATH")]
    ssh_key: Option<PathBuf>,

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
    /// Forward SSH keys into the sandbox for git operations.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    ssh: bool,
}

fn main() -> io::Result<()> {
    let args = Cli::parse();

    if args.list {
        display_active_workspaces()?;
        return Ok(());
    }

    if args.build_image {
        return build_images(&args.agents);
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

    let mut local_ssh = read_local_ssh(&current_dir);
    if args.ssh
        && !local_ssh
        && io::stdin().is_terminal()
        && prompt_enable_persistent_ssh(&current_dir, &workspace_name)?
    {
        local_ssh = true;
    }
    let forward_ssh = !args.no_credentials && (args.ssh || local_ssh || args.ssh_key.is_some());

    execute_sandbox(
        &workspace_name,
        &current_dir,
        mapped_dirs,
        &command_to_run,
        &SessionPolicy {
            credentials: !args.no_credentials,
            ssh: forward_ssh,
            ssh_key: args.ssh_key.as_deref(),
            restrict_net: args.restrict_net,
        },
    )?;
    Ok(())
}

pub(crate) fn dirs_home_dir() -> Option<PathBuf> {
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

fn read_local_ssh(current_dir: &Path) -> bool {
    fs::read_to_string(current_dir.join(LOCAL_CONFIG_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str::<LocalConfig>(&raw).ok())
        .map(|c| c.ssh)
        .unwrap_or_default()
}

fn save_local_ssh(current_dir: &Path, workspace: &str, enabled: bool) -> io::Result<()> {
    let local_config_path = current_dir.join(LOCAL_CONFIG_FILE);
    let mut config: LocalConfig = if local_config_path.exists() {
        let file = File::open(&local_config_path)?;
        serde_json::from_reader(file).unwrap_or(LocalConfig {
            workspace: workspace.to_string(),
            allow: Vec::new(),
            ssh: false,
        })
    } else {
        LocalConfig {
            workspace: workspace.to_string(),
            allow: Vec::new(),
            ssh: false,
        }
    };

    config.ssh = enabled;
    let local_file = File::create(&local_config_path)?;
    serde_json::to_writer_pretty(local_file, &config).map_err(io::Error::other)?;
    Ok(())
}

fn prompt_enable_persistent_ssh(current_dir: &Path, workspace: &str) -> io::Result<bool> {
    print!(
        "[?] Enable SSH forwarding permanently for workspace '{}' in .geli.json? (y/N): ",
        workspace
    );
    io::stdout().flush()?;

    let mut choice = String::new();
    io::stdin().lock().read_line(&mut choice)?;
    let trimmed = choice.trim().to_lowercase();

    if trimmed == "y" || trimmed == "yes" {
        save_local_ssh(current_dir, workspace, true)?;
        println!("[+] Saved: SSH is now enabled permanently for this workspace.\n");
        return Ok(true);
    }

    Ok(false)
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
        ssh: false,
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

mod agents;
mod guest;
mod net;
mod ui;
#[cfg(target_os = "linux")]
mod qemu;

use agents::*;
use guest::*;
use net::*;
use ui::*;
#[cfg(target_os = "linux")]
use qemu::*;


/// Provision one image by booting a VM that runs a cloud-init recipe and powers itself off.
///
/// Shared by the base image and every agent layer, which differ only in their recipe and in what
/// they hand back. Returns the directory the guest wrote its outputs to; `Ok(None)` means the
/// guest did not print the sentinel, so whatever it produced must not be published.
///
/// Builds into `<name>.building` and the caller renames on success: a failed build must never
/// leave a half-provisioned image standing where a working one was.
#[cfg(target_os = "linux")]
fn run_build_vm(
    pending: &Path,
    recipe: &str,
    instance: &str,
    log_path: &Path,
) -> io::Result<Option<PathBuf>> {
    let stage = staging_dir(&format!("geli-stage-{}-{}", std::process::id(), instance))?;
    let iso = make_cloud_init_iso(&stage, recipe, instance, "geli-build")?;

    // The build VM hands the kernel, initramfs and metadata back through this share.
    let out_dir = staging_dir(&format!("geli-out-{}-{}", std::process::id(), instance))?;
    let out_share = vec![
        "-fsdev".to_string(),
        format!(
            "local,path={},id={},security_model=none",
            out_dir.display(),
            BUILD_OUT_TAG
        ),
        "-device".to_string(),
        format!("virtio-9p-pci,fsdev={},mount_tag={}", BUILD_OUT_TAG, BUILD_OUT_TAG),
    ];

    run_qemu(pending, &iso, out_share, QemuIo::LogTo(log_path.to_path_buf()), None)?.wait()?;
    let _ = fs::remove_dir_all(&stage);

    // cloud-init does not abort runcmd on failure, so "the build finished" proves nothing. The
    // recipe echoes the sentinel only behind a check that every tool is really there.
    let console = fs::read_to_string(log_path).unwrap_or_default();
    if console.contains(BUILD_OK_MARKER) {
        Ok(Some(out_dir))
    } else {
        let _ = fs::remove_dir_all(&out_dir);
        Ok(None)
    }
}

#[cfg(target_os = "linux")]
fn build_failed(pending: &Path, log_path: &Path, what: &str) -> ! {
    let _ = fs::remove_file(pending);
    eprintln!("\n[!] Building {} failed: the guest did not report it ready.", what);
    eprintln!("    Console log kept at {}", log_path.display());
    std::process::exit(1);
}

/// Provision the base image: the toolchain, the sandbox user, autologin — and no agent.
#[cfg(target_os = "linux")]
fn build_base_image(dir: &Path) -> io::Result<()> {
    // GELI_BASE_IMAGE points the build at a different cloud image — for comparing distros. The
    // recipe assumes apk and cloud-init, so Alpine-family images work; others need it changed.
    let cloud_img = match std::env::var_os("GELI_BASE_IMAGE") {
        Some(path) => PathBuf::from(path),
        None => dir.join(CLOUD_IMAGE_NAME),
    };
    if !cloud_img.exists() {
        eprintln!("Error: Cloud image not found at {}", cloud_img.display());
        eprintln!("Run ./setup.sh, or place the base cloud image in {}", dir.display());
        std::process::exit(1);
    }

    let recipe = build_base_cloud_init(host_uid());
    let target = dir.join(BASE_IMAGE_NAME);
    let pending = dir.join(format!("{}.building", BASE_IMAGE_NAME));
    let log_path = dir.join(BASE_LOG_NAME);

    create_overlay(&cloud_img, &pending, Some(SANDBOX_DISK_SIZE))?;

    println!("[*] Building the base image: bash, Node {REQUIRED_NODE_MAJOR}, python3, pip, git.");
    println!("[*] No agent goes in here — each one is its own layer on top.");
    println!("[*] Follow along with:  tail -f {}", log_path.display());

    let Some(out_dir) = run_build_vm(&pending, &recipe, "geli-base", &log_path)? else {
        build_failed(&pending, &log_path, "the base image");
    };

    // Direct boot is useless without these, so a build that did not produce them is a failure
    // even if the guest reported every tool present.
    for name in [KERNEL_NAME, INITRD_NAME, BASE_META_NAME] {
        let produced = out_dir.join(name);
        if !produced.exists() {
            let _ = fs::remove_file(&pending);
            eprintln!("\n[!] Base image build failed: the guest did not hand back {}.", name);
            eprintln!("    Console log kept at {}", log_path.display());
            std::process::exit(1);
        }
        fs::copy(&produced, dir.join(name))?;
    }

    fs::rename(&pending, &target)?;
    fs::write(dir.join(BASE_RECIPE_NAME), recipe_hash(&recipe))?;
    let _ = fs::remove_dir_all(&out_dir);

    // An agent layer built against the old base is not merely stale, it is wrong: it sits on a
    // backing file that no longer exists. Drop them rather than leave broken chains behind.
    let dropped = discard_layers(dir)?;
    if dropped > 0 {
        println!("[*] Discarded {} agent layer(s) built on the previous base image.", dropped);
    }

    println!("\n[✓] Base image ready at {}", target.display());
    Ok(())
}

/// Remove every agent layer — image, recipe, metadata and console log alike. Called when the base
/// image is rebuilt, since a layer's backing file is gone at that point, and a log describing an
/// image that no longer exists is worse than no log. Safe to run here: it happens after the base
/// build and before any layer build, so it never deletes a log that was just written.
#[cfg(target_os = "linux")]
fn discard_layers(dir: &Path) -> io::Result<usize> {
    let mut dropped = 0;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.starts_with("geli-layer-") {
            if name.ends_with(".qcow2") {
                dropped += 1;
            }
            let _ = fs::remove_file(&path);
        }
    }
    Ok(dropped)
}

/// The recipe hash a layer must record to be considered current: its parent's hash plus its own
/// recipe. So editing one agent's TOML invalidates that layer and everything stacked on it, and
/// nothing else.
#[cfg(target_os = "linux")]
fn layer_hash(parent_hash: &str, recipe: &str) -> String {
    recipe_hash(&format!("{}\n{}", parent_hash, recipe))
}

/// Build the qcow2 layer for `agent` on top of `parent`, if it is missing or stale.
///
/// Returns the layer's path and its recipe hash, so the next agent in the chain can stack on it.
///
/// Safe to run from several shells at once, which matters because layers are built on *first use*:
/// two tmux panes starting the same agent for the first time is the ordinary way to hit this. One
/// takes the lock and builds; the others wait and then find the layer already there.
#[cfg(target_os = "linux")]
fn ensure_layer(
    dir: &Path,
    agent: &Agent,
    parent: &Path,
    parent_hash: &str,
) -> io::Result<(PathBuf, String)> {
    // Every name this layer owns comes from the agent's command, so there is no separate key that
    // could disagree with the agent being installed.
    let key = agent.command.as_str();
    let recipe = build_layer_cloud_init(agent, host_uid());
    let hash = layer_hash(parent_hash, &recipe);

    let target = dir.join(layer_image_name(key));
    let recipe_path = dir.join(layer_recipe_name(key));
    let lock_path = dir.join(layer_lock_name(key));

    // Checked inside the loop, not before it: a waiter's whole purpose is to re-read this after
    // the other process has published its work.
    let current = || {
        target.exists()
            && fs::read_to_string(&recipe_path).unwrap_or_default().trim() == hash
    };

    let _lock = loop {
        if current() {
            return Ok((target, hash));
        }
        match try_build_lock(&lock_path)? {
            Some(lock) => break lock,
            None => {
                if !wait_for_builder(&lock_path, &agent.command, std::time::Duration::from_secs(1800)) {
                    eprintln!(
                        "\n[!] Gave up waiting for another geli to build the {} layer.",
                        agent.command
                    );
                    eprintln!("    If nothing is building, remove {}", lock_path.display());
                    std::process::exit(1);
                }
            }
        }
    };

    // Keyed by pid as well as by image. The lock already makes a collision impossible, but a
    // shared path meant a crashed build left a file the next one would silently build on top of.
    let pending = dir.join(format!("{}.building.{}", layer_image_name(key), std::process::id()));
    let log_path = dir.join(layer_log_name(key));
    let _ = fs::remove_file(&pending);
    create_overlay(parent, &pending, None)?;

    eprintln!(
        "[*] Building the {} layer, once. Follow along with:  tail -f {}",
        agent.command,
        log_path.display()
    );

    let instance = format!("geli-layer-{}", key);
    let Some(out_dir) = run_build_vm(&pending, &recipe, &instance, &log_path)? else {
        build_failed(&pending, &log_path, &format!("the {} layer", agent.command));
    };

    let meta_name = layer_meta_name(key);
    let produced = out_dir.join(&meta_name);
    if produced.exists() {
        fs::copy(&produced, dir.join(&meta_name))?;
    }
    let _ = fs::remove_dir_all(&out_dir);

    fs::rename(&pending, &target)?;
    fs::write(&recipe_path, &hash)?;
    eprintln!("[✓] {} layer ready.", agent.command);
    Ok((target, hash))
}

/// The image a session should overlay: the invoked agent's layer, built if it is missing, or the
/// base image when the command is not an agent at all.
///
/// One agent, deliberately — see `docs/plans/05-varios-agentes-por-vm.md`. This used to walk a
/// variable-length chain so a session could carry several agents, but nothing ever asked for more
/// than one, and that design was rejected rather than left pending.
///
/// Also returns the metadata the status block prints as the contents of the image: the base's,
/// plus the layer's own entry.
#[cfg(target_os = "linux")]
fn resolve_image(dir: &Path, agent: Option<&Agent>) -> io::Result<(PathBuf, ImageMeta)> {
    let base = dir.join(BASE_IMAGE_NAME);
    let mut meta = parse_image_meta(&fs::read_to_string(dir.join(BASE_META_NAME)).unwrap_or_default());

    // A command geli does not know gets the base image itself — the cheapest boot there is, and
    // one with no agent in it at all.
    let Some(agent) = agent else {
        return Ok((base, meta));
    };

    let base_hash = fs::read_to_string(dir.join(BASE_RECIPE_NAME)).unwrap_or_default().trim().to_string();
    let (layer, _) = ensure_layer(dir, agent, &base, &base_hash)?;
    merge_image_meta(
        &mut meta,
        &fs::read_to_string(dir.join(layer_meta_name(&agent.command))).unwrap_or_default(),
    );

    Ok((layer, meta))
}

/// Whether the base image on disk is the one this binary's recipe describes.
///
/// Checked so `--build-image` is idempotent: `--build-image --agents claude` must be able to add
/// a layer without rebuilding the image that layer is going to sit on — which would also discard
/// every other layer along the way.
#[cfg(target_os = "linux")]
fn base_is_current(dir: &Path) -> bool {
    let recorded = fs::read_to_string(dir.join(BASE_RECIPE_NAME)).unwrap_or_default();
    dir.join(BASE_IMAGE_NAME).exists()
        && recorded.trim() == recipe_hash(&build_base_cloud_init(host_uid()))
}

#[cfg(target_os = "linux")]
fn build_images(selected: &[String]) -> io::Result<()> {
    check_host_tools();
    let dir = images_dir();

    // Anyone upgrading has a ~900 MB single image sitting there that nothing reads any more.
    // Said, not deleted: it is the user's file, and it is the only way back to the old binary.
    let legacy = dir.join(LEGACY_IMAGE_NAME);
    if legacy.exists() {
        // Blocks, not length: a qcow2 is sparse, and `len()` reports the virtual size, which
        // overstates what deleting the file would actually give back by about twofold.
        use std::os::unix::fs::MetadataExt;
        let size = fs::metadata(&legacy).map(|m| m.blocks() * 512 / 1_048_576).unwrap_or(0);
        println!("[*] {} ({} MB) is from before images were layered and is", LEGACY_IMAGE_NAME, size);
        println!("    no longer used. Delete it when you are sure you do not want to go back.");
    }

    if base_is_current(&dir) {
        println!("[*] Base image is already current.");
        println!("    Delete {} to force a rebuild.", dir.join(BASE_IMAGE_NAME).display());
    } else {
        build_base_image(&dir)?;
    }

    // Each named agent gets its own layer directly on the base, not one chain through all of
    // them: a session invokes one agent, and sibling layers are what it can actually reuse.
    for name in selected {
        let Some(agent) = agent_for_command(name) else {
            eprintln!(
                "[!] `{}` is not an agent geli knows ({}). Skipped.",
                name,
                agents().iter().map(|a| a.command.as_str()).collect::<Vec<_>>().join(", ")
            );
            continue;
        };
        resolve_image(&dir, Some(agent))?;
    }

    println!("    Sandbox sessions boot its kernel directly, installing nothing.");
    if selected.is_empty() {
        println!("    An agent's layer is built the first time you run it, or now with:");
        println!(
            "      geli --build-image --agents <{}>",
            agents().iter().map(|a| a.command.as_str()).collect::<Vec<_>>().join("|")
        );
    }
    Ok(())
}

pub(crate) fn collect_ssh_credentials(
    home: &Path,
    ssh_key: Option<&Path>,
) -> io::Result<Vec<(String, String)>> {
    let mut creds = Vec::new();
    let ssh_dir = home.join(".ssh");

    let mut found_any_key = false;
    let mut key_names = Vec::new();

    if let Some(explicit_path) = ssh_key {
        let resolved = if explicit_path.starts_with("~/") || explicit_path == Path::new("~") {
            if let Ok(stripped) = explicit_path.strip_prefix("~") {
                home.join(stripped)
            } else {
                explicit_path.to_path_buf()
            }
        } else if explicit_path.is_relative() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(explicit_path)
        } else {
            explicit_path.to_path_buf()
        };

        // If the user specified a .pub file, prefer the private key counterpart if available
        let resolved = if resolved.extension().and_then(|e| e.to_str()) == Some("pub") {
            let priv_candidate = resolved.with_extension("");
            if priv_candidate.exists() {
                priv_candidate
            } else {
                resolved
            }
        } else {
            resolved
        };

        if !resolved.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("SSH key not found at '{}'", resolved.display()),
            ));
        }

        let content = fs::read_to_string(&resolved)?;
        let key_name = resolved
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("id_custom")
            .to_string();

        creds.push((format!(".ssh/{}", key_name), content));
        key_names.push(key_name.clone());
        found_any_key = true;

        let pub_path = PathBuf::from(format!("{}.pub", resolved.display()));
        if pub_path.exists() {
            if let Ok(pub_content) = fs::read_to_string(&pub_path) {
                creds.push((format!(".ssh/{}.pub", key_name), pub_content));
            }
        }
    } else {
        const DEFAULT_KEYS: &[&str] = &["id_ed25519", "id_ecdsa", "id_rsa"];
        for key in DEFAULT_KEYS {
            let key_path = ssh_dir.join(key);
            if key_path.exists() {
                if let Ok(content) = fs::read_to_string(&key_path) {
                    creds.push((format!(".ssh/{}", key), content));
                    key_names.push(key.to_string());
                    found_any_key = true;

                    let pub_path = ssh_dir.join(format!("{}.pub", key));
                    if pub_path.exists() {
                        if let Ok(pub_content) = fs::read_to_string(&pub_path) {
                            creds.push((format!(".ssh/{}.pub", key), pub_content));
                        }
                    }
                }
            }
        }
    }

    if !found_any_key {
        eprintln!(
            "[!] SSH forwarding was requested, but no SSH private keys were found in {}.\n\
             \x20   Specify a key explicitly with --ssh-key <path>.",
            ssh_dir.display()
        );
    }

    let known_hosts_path = ssh_dir.join("known_hosts");
    let mut has_known_hosts = false;
    if let Ok(known_hosts) = fs::read_to_string(&known_hosts_path) {
        has_known_hosts = !known_hosts.trim().is_empty();
        creds.push((".ssh/known_hosts".to_string(), known_hosts));
    }

    // `accept-new` only where there is nothing to check against. With the host's `known_hosts`
    // forwarded — which is the normal case, and github.com is almost certainly already in it —
    // strict checking costs nothing and keeps the one warning you would get if the host you are
    // pushing to were not the host you think. Blanket `accept-new` trusts whatever answers first,
    // and a sandbox is exactly where an unexpected answer deserves to be noticed.
    //
    // Without `known_hosts` the choice is between `accept-new` and a session that hangs on a
    // prompt nobody is there to answer, so it is `accept-new` and geli says so.
    let strict = if has_known_hosts {
        "yes"
    } else {
        eprintln!(
            "[!] No ~/.ssh/known_hosts to forward, so the sandbox will trust whichever host\n\
             \x20   answers first (StrictHostKeyChecking=accept-new)."
        );
        "accept-new"
    };

    let mut config_lines = vec![
        "Host *".to_string(),
        format!("    StrictHostKeyChecking {}", strict),
    ];
    for name in &key_names {
        config_lines.push(format!("    IdentityFile ~/.ssh/{}", name));
    }
    config_lines.push(String::new());
    creds.push((".ssh/config".to_string(), config_lines.join("\n")));

    Ok(creds)
}

/// What this invocation is allowed to carry into the sandbox, and how far it may reach.
///
/// Grouped rather than passed as four more arguments: they are one decision — how much of the
/// host this session gets — and reading them together is how you see that `--no-credentials`
/// turns off SSH forwarding too.
pub(crate) struct SessionPolicy<'a> {
    /// Copy the invoked agent's credentials out of the user's home.
    pub(crate) credentials: bool,
    /// Copy SSH private keys in as well. Implies `credentials`; see `Cli`.
    pub(crate) ssh: bool,
    /// Forward this key instead of the usual `~/.ssh/id_*`.
    pub(crate) ssh_key: Option<&'a Path>,
    /// Cut egress to the allowlist.
    pub(crate) restrict_net: bool,
}

#[cfg(target_os = "linux")]
fn execute_sandbox(
    ws: &str,
    cur: &Path,
    dirs: Vec<PathBuf>,
    cmd: &str,
    policy: &SessionPolicy,
) -> io::Result<()> {
    let SessionPolicy { credentials: forward_credentials, ssh: forward_ssh, ssh_key, restrict_net } =
        *policy;

    let pid = std::process::id();
    let home = dirs_home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    // Set GELI_KEEP=1 to preserve the session disk and cloud-init files for debugging.
    let keep_session = std::env::var_os("GELI_KEEP").is_some();

    let started = std::time::Instant::now();
    check_host_tools();

    let images = images_dir();
    let base_img = images.join(BASE_IMAGE_NAME);
    if !base_img.exists() {
        eprintln!("\n[!] Error: Base image not found at {}", base_img.display());
        if images.join(LEGACY_IMAGE_NAME).exists() {
            eprintln!(
                "{} is from before images were layered and cannot be used.",
                LEGACY_IMAGE_NAME
            );
            eprintln!("Delete it and rebuild — the new base image carries no agents, so it is");
            eprintln!("smaller, and each agent becomes its own layer.");
        }
        eprintln!("Build it once with:\n  geli --build-image\n");
        std::process::exit(1);
    }

    let meta_path = images.join(BASE_META_NAME);
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

    // Stale images still boot — they are merely out of date, not broken.
    let expected = recipe_hash(&build_base_cloud_init(host_uid()));
    let recorded = fs::read_to_string(images.join(BASE_RECIPE_NAME)).unwrap_or_default();
    if recorded.trim() != expected {
        eprintln!("[!] The base image was built from a different recipe than this binary expects.");
        eprintln!("    Rebuild when convenient:  geli --build-image");
    }

    let agent = agent_for_command(cmd);

    // The base image carries no agent, so the invoked one's layer has to exist before the
    // session can overlay anything. Missing means first use: build it now, once, and say so —
    // this is the only time geli takes minutes instead of seconds.
    let (session_backing, meta) = resolve_image(&images, agent)?;

    let host_cache_dir = home.join(".cache").join("geli-sandbox");
    let npm_cache = host_cache_dir.join("npm");
    let pip_cache = host_cache_dir.join("pip");
    fs::create_dir_all(&npm_cache)?;
    fs::create_dir_all(&pip_cache)?;

    let vm_share_dir = staging_dir(&format!("sandbox-share-{}", pid))?;
    let sandbox_img = PathBuf::from(format!("/tmp/sandbox-session-{}.qcow2", pid));

    let cur_canon = cur.canonicalize()?;

    // The proxy has to exist before the guest's own configuration is generated, not just before
    // it boots: the egress ruleset names the one port the guest may reach, and that port is
    // whatever the OS handed the listener. So this comes before `build_mount_script`.
    let proxy = if restrict_net {
        let allow = session_allowlist(agent, &read_local_allowlist(cur));
        let count = allow.len();
        Some((start_proxy(allow, vm_share_dir.join("net.log"))?, count))
    } else {
        None
    };
    let proxy_port = proxy.as_ref().map(|(p, _)| p.port);

    let plan = build_mount_script(&dirs, &cur_canon, &build_lockdown(proxy_port));

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

    // `agent` and the image chain were resolved above, before anything was overlaid.
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    let openai_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();

    // Only the invoked agent's credentials travel, and only the credential — not the history
    // sitting beside it. Opt out entirely with --no-credentials.
    let agent_credentials: Vec<(String, String)> = match (forward_credentials, agent) {
        (true, Some(agent)) => agent
            .credentials
            .iter()
            .filter_map(|rel| {
                fs::read_to_string(home.join(rel)).ok().map(|c| (rel.to_string(), c))
            })
            .collect(),
        _ => Vec::new(),
    };

    let ssh_credentials = if forward_ssh {
        match collect_ssh_credentials(&home, ssh_key) {
            Ok(creds) => creds,
            Err(e) => {
                eprintln!("[!] Error reading SSH credentials: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        Vec::new()
    };

    // Not "may be blocked": the egress ruleset is default-drop with a single rule for the proxy's
    // port, so nothing reaches port 22, and git over SSH does not read the proxy variables the way
    // curl and npm do. Saying "may" would send someone hunting for a configuration that does not
    // exist. The keys are still worth forwarding here — SSH commit signing is entirely local.
    if forward_ssh && restrict_net {
        eprintln!(
            "[!] --restrict-net blocks SSH outright: egress is default-drop except the proxy's\n\
             \x20   port, and git over SSH ignores the proxy variables. Pushing and fetching over\n\
             \x20   ssh:// will fail. The forwarded keys still work for local use, such as\n\
             \x20   signing commits. Use an https:// remote to push under --restrict-net.\n"
        );
    }

    let mut credentials = agent_credentials.clone();
    credentials.extend(ssh_credentials.clone());

    let has_credentials = !anthropic_key.trim().is_empty()
        || !openai_key.trim().is_empty()
        || !agent_credentials.is_empty();

    if let Some(agent) = agent {
        let risky = sensitive_credentials(agent);
        if !risky.is_empty() {
            eprintln!(
                "\n[!] The `{}` recipe asks to copy {} out of your home.\n\
                 \x20   Those are not agent credentials. Check agents/{}.toml before trusting it,\n\
                 \x20   or run with --no-credentials.\n",
                agent.command,
                risky.join(", "),
                agent.command
            );
        }
    }

    if let Some(notice) = non_agent_notice(cmd) {
        eprintln!("{}", notice);
    }
    if let Some(warning) = credential_warning(cmd, has_credentials) {
        eprintln!("\n{}\n", warning);
    }

    let api_key_for_config = anthropic_key.clone();

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

    // Not gated on --no-credentials: these are built from this sandbox's own workspace, not read
    // from the user's home, so there is nothing to opt out of. And an agent running on a bare
    // ANTHROPIC_API_KEY still needs its onboarding answered.
    let seed = match agent {
        Some(a) => seed_files(a, &plan.folders, &api_key_for_config),
        None => Vec::new(),
    };

    let user_data = build_cloud_init(
        &plan,
        cmd,
        &env_exports,
        &credentials,
        &seed,
        proxy_port,
    );
    let iso = make_cloud_init_iso(
        &vm_share_dir,
        &user_data,
        &format!("geli-{}", pid),
        &format!("sandbox-{}", ws),
    )?;

    // The session overlay sits on top of the agent layer (or the base image, for a command that
    // is not an agent) and inherits its virtual size.
    create_overlay(&session_backing, &sandbox_img, None)?;

    let base_auth = match (agent_credentials.is_empty(), agent) {
        (false, Some(a)) => format!("{} · bills your plan", a.label),
        (true, _) if !api_key_for_config.trim().is_empty() => {
            "ANTHROPIC_API_KEY · bills API credits".to_string()
        }
        _ => "none".to_string(),
    };
    let auth = if forward_ssh && !ssh_credentials.is_empty() {
        if base_auth == "none" {
            "SSH keys forwarded".to_string()
        } else {
            format!("{} + SSH", base_auth)
        }
    } else {
        base_auth
    };
    let net = match &proxy {
        Some((_, count)) => format!("restricted · {} domains allowed", count),
        None => "open · unrestricted".to_string(),
    };
    eprint!(
        "{}",
        render_status(
            ws,
            &status_mounts,
            &auth,
            &credentials.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
            &describe_image(&meta),
            &net,
        )
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
        let animate = std::io::stderr().is_terminal();
        let color = animate && std::env::var_os("NO_COLOR").is_none();
        track_boot(
            &console_log,
            &mut child,
            animate,
            color,
            std::time::Duration::from_secs(90),
        )
    };

    if !ready {
        eprintln!("[!] The sandbox never reported ready.");
        // The reason is almost always the last thing the guest said, and sending the user to a
        // file that is about to be deleted on exit is not help. Show the tail here.
        let log = fs::read_to_string(&console_log).unwrap_or_default();
        let tail: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).rev().take(6).collect();
        for line in tail.iter().rev() {
            eprintln!("    | {}", line.trim_end());
        }
        eprintln!("    Full console log: {}", console_log.display());
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
fn build_images(_agents: &[String]) -> io::Result<()> {
    eprintln!("Image building is only implemented on Linux.");
    Ok(())
}

#[cfg(target_os = "macos")]
fn execute_sandbox(
    _ws: &str,
    _cur: &Path,
    _dirs: Vec<PathBuf>,
    _cmd: &str,
    _policy: &SessionPolicy,
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
    _policy: &SessionPolicy,
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
        let seed = vec![(".claude.json".to_string(), build_claude_config(&plan.folders, "sk-ant-test-key-0123456789"))];
        build_cloud_init(&plan, cmd, &env, &[], &seed, None)
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
    fn base_cloud_init_parses() {
        let yaml = build_base_cloud_init(1000);
        let parsed = YamlLoader::load_from_str(&yaml);
        assert!(parsed.is_ok(), "{:?}\n---\n{}", parsed.err(), yaml);
    }

    /// The whole point of the golden image: a session must not install anything.
    /// A restricted session carries one more `write_files` entry, and the ruleset inside it has
    /// braces and semicolons of its own. Unparsed cloud-init fails *silently* — the VM boots and
    /// the command never runs — so the restricted document needs its own case.
    #[test]
    fn restricted_session_cloud_init_parses() {
        let dirs = [PathBuf::from("/home/u/proj")];
        let plan = build_mount_script(&dirs, Path::new("/home/u/proj"), &build_lockdown(Some(45678)));
        let seed = vec![(".claude.json".to_string(), build_claude_config(&plan.folders, "sk-ant-test"))];
        let creds = vec![(".claude/.credentials.json".to_string(), "{\"t\":1}".to_string())];
        let yaml = build_cloud_init(&plan, "claude", "", &creds, &seed, Some(45678));

        let parsed = YamlLoader::load_from_str(&yaml);
        assert!(parsed.is_ok(), "{:?}\n---\n{}", parsed.err(), yaml);

        // And the ruleset has to have actually survived into the document, not just parsed.
        assert!(yaml.contains("/etc/geli/egress.nft"));
        assert!(yaml.contains("tcp dport 45678 accept"));
    }

    #[test]
    fn session_cloud_init_installs_nothing() {
        let yaml = session_cloud_init(&["/home/u/proj"], "/home/u/proj", "claude");
        assert!(!yaml.contains("apt-get"), "session still runs apt:\n{}", yaml);
        assert!(!yaml.contains("npm install"), "session still runs npm:\n{}", yaml);
        assert!(!yaml.contains("package_update"));
    }

    #[test]
    fn base_cloud_init_bakes_tooling_and_login() {
        let yaml = build_base_cloud_init(1000);
        for expected in ["git", "geli-autologin"] {
            assert!(yaml.contains(expected), "base recipe missing {:?}", expected);
        }
        // The marker must be guarded by a real check, not echoed unconditionally. The agents
        // moved out to their own layers, so what the base proves is the toolchain.
        assert!(yaml.contains("command -v node"));
        assert!(yaml.contains(BUILD_OK_MARKER));

        // Alpine ships Node 22, so no tarball — but the version is still checked, because
        // npm installs onto a too-old runtime with only a warning.
        assert!(yaml.contains("apk add"));
        // Every network step must be retried: a single dropped TLS handshake would otherwise
        // fail the whole build under `set -e`.
        assert!(yaml.contains("retry apk update"));
        assert!(yaml.contains("retry apk add"));
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
        assert!(LOGIN_PROFILE.contains("cloud-init status --wait"));
        assert!(LOGIN_PROFILE.contains("/etc/geli/session"));
        // Sourcing ~/.bashrc is fine; appending the command to it is not.
        assert!(!LOGIN_PROFILE.contains(">> ~/.bashrc"));
    }

    /// Regression: Claude Code's Bash tool spawns login shells, which read .bash_profile. Without
    /// a guard, every command the agent ran re-entered the session and hit `sudo poweroff`,
    /// shutting the VM down mid-task.
    /// Regression: Alpine's default user takes uid 1000, pushing `sandbox` to 1001. Files
    /// arrive over 9p owned by the host user, so the agent could read the project but not
    /// write to it.
    #[test]
    fn base_user_takes_the_host_uid() {
        let yaml = build_base_cloud_init(1000);
        assert!(yaml.contains("adduser -D -u 1000"));
        // Alpine's own default user holds uid 1000 and has to go, or sandbox lands on 1001.
        assert!(yaml.contains("deluser alpine"));
        // The stock image waits 10s at a boot menu nobody is there to answer.
        assert!(yaml.contains("TIMEOUT 1"), "boot menu timeout not disabled");

        let other = build_base_cloud_init(1234);
        assert!(other.contains("adduser -D -u 1234"));
        // A different uid must produce a different image, or stale images go undetected.
        assert_ne!(recipe_hash(&yaml), recipe_hash(&other));
    }

    #[test]
    fn login_profile_runs_the_session_only_once() {
        assert!(LOGIN_PROFILE.contains("GELI_SESSION_ACTIVE"));
        assert!(LOGIN_PROFILE.contains("/tmp/.geli-session-active"));

        // The poweroff must sit inside the guard, never at top level.
        let guard = LOGIN_PROFILE
            .find("GELI_SESSION_ACTIVE")
            .expect("guard missing");
        let poweroff = LOGIN_PROFILE.find("sudo poweroff").expect("poweroff missing");
        assert!(poweroff > guard, "poweroff runs before the guard");

        // Both branches source the environment, or the agent's commands lose TERM and keys.
        assert_eq!(LOGIN_PROFILE.matches(". /etc/geli/env").count(), 2);
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
    let recipe = build_base_cloud_init(host_uid);
        assert_eq!(recipe_hash(&recipe), recipe_hash(&recipe));
        assert_ne!(recipe_hash(&recipe), recipe_hash(&format!("{}\n# extra", recipe)));
    }

    /// Regression: `geli claude` with no key booted fine and then sat silently in the agent's
    /// first-run login flow, which is indistinguishable from a broken sandbox.
    #[test]
    fn warns_before_booting_an_agent_with_no_credentials() {
        // The warning names the agent and the path it looked in, so the user can act on it.
        let warning = credential_warning("claude", false).expect("expected a warning");
        assert!(warning.contains("claude"));
        assert!(warning.contains("~/.claude/.credentials.json"));
        assert!(warning.contains("first-run login"));

        let agy = credential_warning("agy -p hi", false).unwrap();
        assert!(agy.contains("~/.gemini/oauth_creds.json"));
        assert!(!agy.contains(".claude"), "it named another agent's credential");

        assert!(credential_warning("claude", true).is_none());

        // Unrelated commands still get a note, but not the agent-specific explanation.
        let generic = credential_warning("ls -la", false).unwrap();
        assert!(!generic.contains("first-run login"));
    }

    /// A notice, not a refusal: running a plain shell in the sandbox is a legitimate thing to do.
    /// A recipe is a file anyone can contribute, and `credentials` copies files out of the
    /// user's home. Paths that are plainly not agent credentials have to be surfaced.
    #[test]
    fn recipes_asking_for_secrets_are_flagged() {
        for agent in agents() {
            assert!(
                sensitive_credentials(agent).is_empty(),
                "shipped recipe {} asks for something it should not",
                agent.command
            );
        }

        let greedy = Agent {
            command: "greedy".into(),
            binary: "greedy".into(),
            label: "x".into(),
            credentials: vec![
                ".config/greedy/auth.json".into(),
                ".ssh/id_rsa".into(),
                ".aws/credentials".into(),
            ],
            hosts: vec![],
            install: String::new(),
            version: None,
        };
        let flagged = sensitive_credentials(&greedy);
        assert_eq!(flagged, vec![".ssh/id_rsa", ".aws/credentials"]);
        assert!(!flagged.contains(&".config/greedy/auth.json"));
    }

    #[test]
    fn non_agent_commands_are_noticed_not_refused() {
        let notice = non_agent_notice("bash -lc make").expect("expected a notice");
        assert!(notice.contains("bash"));
        assert!(notice.contains("claude"), "it should list the agents geli does know");
        assert!(notice.contains("will run"), "it must not read as a refusal");

        // Known agents and an empty command say nothing.
        for quiet in ["claude", "/usr/local/bin/agy --version", "opencode", ""] {
            assert!(non_agent_notice(quiet).is_none(), "{:?} should be silent", quiet);
        }
    }

    #[test]
    fn agent_is_recognised_however_it_is_pathed() {
        assert_eq!(agent_for_command("claude").map(|a| a.command.as_str()), Some("claude"));
        assert_eq!(agent_for_command("claude --resume").map(|a| a.command.as_str()), Some("claude"));
        assert_eq!(agent_for_command("/usr/local/bin/agy -p hi").map(|a| a.command.as_str()), Some("agy"));
        assert_eq!(agent_for_command("opencode").map(|a| a.command.as_str()), Some("opencode"));

        // Near-misses must not match, or the wrong credentials would travel.
        for not_an_agent in ["claudette", "echo claude", "", "agyx", "my-opencode"] {
            assert!(agent_for_command(not_an_agent).is_none(), "{:?} matched", not_an_agent);
        }
    }

    /// Each agent carries only its own credential. Running one agent must not put another
    /// service's token in the guest.
    #[test]
    fn agents_declare_only_their_own_credentials() {
        let claude = agent_for_command("claude").unwrap();
        assert_eq!(claude.credentials, vec![".claude/.credentials.json".to_string()]);

        let agy = agent_for_command("agy").unwrap();
        // Tokens live beside 2.3 GB of conversation history and logs.
        assert!(agy.credentials.iter().all(|p| p.starts_with(".gemini/")));
        for history in ["brain", "conversations", "history.jsonl", "conversation_summaries.db"] {
            assert!(
                !agy.credentials.iter().any(|p| p.contains(history)),
                "{} would drag history into the guest",
                history
            );
        }
        assert!(agy.credentials.contains(&".gemini/antigravity-cli/antigravity-oauth-token".to_string()));

        // No agent may claim another's files.
        for a in agents() {
            for b in agents() {
                if a.command != b.command {
                    assert!(
                        a.credentials.iter().all(|p| !b.credentials.contains(p)),
                        "{} and {} share a credential path",
                        a.command,
                        b.command
                    );
                }
            }
        }
    }

    #[test]
    fn allowlist_opens_only_the_invoked_agents_hosts() {
        let for_claude = session_allowlist(agent_for_command("claude"), &[]);
        assert!(for_claude.contains(&"api.anthropic.com".to_string()));
        assert!(!for_claude.iter().any(|h| h.contains("googleapis")));

        let for_agy = session_allowlist(agent_for_command("agy"), &[]);
        assert!(for_agy.iter().any(|h| h.contains("googleapis")));
        assert!(!for_agy.contains(&"api.anthropic.com".to_string()));

        // Registries are common ground: every agent needs to install things.
        for list in [&for_claude, &for_agy] {
            assert!(list.contains(&"registry.npmjs.org".to_string()));
        }
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
        let yaml = build_cloud_init(&plan, "claude", &env, &[], &[], None);

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
        let creds = vec![(
            ".claude/.credentials.json".to_string(),
            r#"{"claudeAiOauth":{"accessToken":"tok","refreshToken":"ref"}}"#.to_string(),
        )];
        let yaml = build_cloud_init(&plan, "claude", "", &creds, &[], None);

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

    /// Not a real test: writes every generated guest document to /tmp so a refactor can be
    /// proven byte-identical. Run with `cargo test dump_generated -- --ignored`.
    #[ignore]
    #[test]
    fn dump_generated_documents() {
        let dir = std::path::PathBuf::from("/tmp/geli-baseline");
        std::fs::create_dir_all(&dir).unwrap();

        let dirs = [PathBuf::from("/home/u/proj"), PathBuf::from("/home/u/api")];
        let plan = build_mount_script(&dirs, Path::new("/home/u/proj"), &build_lockdown(Some(45678)));
        let env = format!(
            "{}\n{}\n{}",
            build_env_exports(&[("ANTHROPIC_API_KEY", "sk-fixed".to_string())]),
            build_proxy_env(Some(45678)),
            build_terminal_setup("xterm-256color", "truecolor", Some((46, 190)))
        );
        let seed = vec![(".claude.json".to_string(), build_claude_config(&plan.folders, "sk-ant-fixed-0123456789"))];
        let creds = vec![(".claude/.credentials.json".to_string(), "{\"t\":1}".to_string())];

        let files: Vec<(&str, String)> = vec![
            ("base.cloud-init.yaml", build_base_cloud_init(1000)),
            ("session.cloud-init.yaml", build_cloud_init(&plan, "claude", &env, &creds, &seed, Some(45678))),
            ("setup.sh", base_setup_script(1000)),
            ("verify.sh", base_verify_script()),
            ("profile.sh", LOGIN_PROFILE.to_string()),
            ("autologin.sh", AUTOLOGIN_HELPER.to_string()),
            ("mounts.sh", plan.script.clone()),
            ("claude.json", seed[0].1.clone()),
            ("lockdown.sh", build_lockdown(Some(45678))),
            ("egress.nft", build_egress_ruleset(45678)),
        ];
        // One per agent: a layer recipe is the only place an agent's install reaches the guest.
        let mut files = files;
        for agent in agents() {
            files.push((
                // Leaked into the loop's lifetime on purpose — this is a dump, not a test.
                Box::leak(format!("layer.{}.cloud-init.yaml", agent.command).into_boxed_str()),
                build_layer_cloud_init(agent, 1000),
            ));
        }
        for (name, body) in files {
            std::fs::write(dir.join(name), body).unwrap();
        }
    }

    /// Not a real test: writes the agent table out as TOML so the files that replace it are
    /// byte-faithful to what the Rust version said.
    #[ignore]
    #[test]
    fn dump_agents_as_toml() {
        for a in agents() {
            let mut out = String::new();
            out.push_str(&format!("command = {:?}\n", a.command));
            out.push_str(&format!("binary = {:?}\n", a.binary));
            out.push_str(&format!("label = {:?}\n", a.label));
            out.push_str(&format!(
                "credentials = [{}]\n",
                a.credentials.iter().map(|c| format!("{:?}", c)).collect::<Vec<_>>().join(", ")
            ));
            out.push_str(&format!(
                "hosts = [\n{}\n]\n",
                a.hosts.iter().map(|h| format!("  {:?},", h)).collect::<Vec<_>>().join("\n")
            ));
            out.push_str(&format!("install = '''\n{}'''\n", a.install));
            std::fs::write(format!("agents/{}.toml", a.command), out).unwrap();
        }
    }

    // --- egress policy ---

    /// The proxy must survive being used more than once. A blocked host is enough to exercise
    /// accept → parse → refuse without touching the real network.
    #[cfg(target_os = "linux")]
    #[test]
    fn proxy_serves_more_than_one_connection() {
        use std::io::{Read, Write};

        let log = std::env::temp_dir().join(format!("geli-proxy-{}.log", std::process::id()));
        let proxy = net::start_proxy(vec!["allowed.example".to_string()], log.clone())
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
        session_allowlist(
            agent_for_command("claude"),
            &["*.internal.example".to_string(), ".corp.test".to_string()],
        )
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

    /// All three parts of the lockdown are load-bearing. Dropping the route cuts what is off-link;
    /// the ruleset cuts what is still on-link, DNS above all; narrowing sudo is what stops the
    /// agent — root in the guest by default — from undoing either.
    #[test]
    fn lockdown_cuts_the_route_the_resolver_and_root() {
        assert_eq!(build_lockdown(None), "", "an open session must not be touched");

        let locked = build_lockdown(Some(45678));
        assert!(locked.contains("ip route del default"));
        assert!(locked.contains("nft -f /etc/geli/egress.nft"));
        assert!(locked.contains("NOPASSWD: /sbin/poweroff"));
        assert!(
            !locked.contains("NOPASSWD: ALL"),
            "leaving blanket sudo would let the agent undo the route"
        );

        // The status block has already told the user egress is restricted by the time this runs.
        // Carrying on with an unapplied ruleset would make that a lie, so it has to be fatal.
        let fail = locked.find("if ! nft -f").expect("the ruleset must be applied conditionally");
        let off = locked.find("poweroff -f").expect("a failed ruleset must stop the session");
        assert!(fail < off);
    }

    /// Sudo is narrowed *after* the route and the ruleset, because the agent is root until then.
    /// Narrowing first would leave the two steps that matter running with less privilege than
    /// they need; narrowing last is what makes them stick.
    #[test]
    fn lockdown_takes_privilege_away_last() {
        let locked = build_lockdown(Some(45678));
        let route = locked.find("ip route del default").unwrap();
        let nft = locked.find("nft -f").unwrap();
        let sudo = locked.find("NOPASSWD: /sbin/poweroff").unwrap();
        assert!(route < sudo && nft < sudo);
    }

    /// The ruleset's whole allowance is one TCP port on the host alias. A rule naming the host
    /// without the port would be a tunnel out through anything else listening there.
    #[test]
    fn egress_ruleset_opens_only_the_proxy_port() {
        let rules = build_egress_ruleset(45678);
        assert!(rules.contains("policy drop"));
        assert!(rules.contains("ip daddr 10.0.2.2 tcp dport 45678 accept"));
        assert!(rules.contains(r#"oifname "lo" accept"#), "the agent's own loopback must survive");

        // The resolver is the reason this file exists; it must not appear as an accept rule.
        for line in rules.lines().filter(|l| l.trim_start().starts_with("ip daddr")) {
            assert!(!line.contains(GUEST_DNS), "the resolver must not be reachable: {}", line);
        }
        // `inet`, not `ip`: a v4-only table would leave IPv6 egress wide open.
        assert!(rules.contains("table inet geli"));

        assert_eq!(build_egress_entry(None), "", "an open session gets no policy file at all");
        assert!(build_egress_entry(Some(45678)).contains("/etc/geli/egress.nft"));
    }

    /// The lockdown has to be in place before the agent runs, and the readiness marker is what
    /// tells the host the guest is ready — so the lockdown must come first.
    #[test]
    fn lockdown_runs_before_the_session_is_declared_ready() {
        let plan = build_mount_script(
            &[PathBuf::from("/home/u/proj")],
            Path::new("/home/u/proj"),
            &build_lockdown(Some(45678)),
        );
        let ready = plan.script.find(READY_MARKER).expect("no ready marker");
        for step in ["ip route del default", "nft -f /etc/geli/egress.nft", "/sbin/poweroff"] {
            let at = plan.script.find(step).unwrap_or_else(|| panic!("no {} in the script", step));
            assert!(at < ready, "the agent could run before `{}`", step);
        }
    }

    #[test]
    fn session_allowlist_adds_without_duplicating() {
        let claude = agent_for_command("claude");
        let base = session_allowlist(claude, &[]);
        assert!(base.contains(&"api.anthropic.com".to_string()));

        let extended = session_allowlist(claude, &[
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

        // An empty allowlist or false ssh flag must not be written back into users' files.
        let written = serde_json::to_string(&LocalConfig {
            workspace: "w".into(),
            allow: Vec::new(),
            ssh: false,
        })
        .unwrap();
        assert!(!written.contains("allow"));
        assert!(!written.contains("ssh"));

        let ssh_cfg: LocalConfig = serde_json::from_str(r#"{"workspace":"w","ssh":true}"#).unwrap();
        assert!(ssh_cfg.ssh);
        let written_ssh = serde_json::to_string(&ssh_cfg).unwrap();
        assert!(written_ssh.contains("\"ssh\":true"));
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

    #[test]
    fn status_block_shows_mounts_auth_and_image() {
        let mounts = vec![
            StatusMount { host: "/home/u/proj".into(), guest: "/workspace/proj".into(), active: true },
            StatusMount { host: "/home/u/api".into(), guest: "/workspace/api".into(), active: false },
        ];
        let copied = vec![".claude/.credentials.json".to_string()];
        let out = render_status("acme", &mounts, "claude.ai credentials", &copied, "alpine 3.22", "open");

        assert!(out.starts_with("geli · workspace acme\n"));
        assert!(out.contains("/home/u/proj → /workspace/proj  (active)"));
        assert!(out.contains("/home/u/api → /workspace/api\n"));
        assert!(out.contains("auth   claude.ai credentials"));
        assert!(out.contains("image  alpine 3.22"));
        // The network posture is a security property that changes per run: always shown.
        assert!(out.contains("net    open"));
        // The files that leave the user's home are named, not summarised.
        assert!(out.contains("~/.claude/.credentials.json"));
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
    fn base_recipe_silences_the_guest_and_hands_out_the_kernel() {
        let yaml = build_base_cloud_init(1000);
        // Nothing of the distro's own chatter should reach a clean session.
        assert!(yaml.contains("rm -f /etc/motd"));
        assert!(yaml.contains("/etc/issue"));
        // `getty -n` writes a CRLF before handing over, a blank line on the user's stdout.
        assert!(yaml.contains("ttyS0::respawn:/usr/local/bin/geli-autologin"));
        assert!(!yaml.contains("/sbin/getty"), "autologin went back through getty");
        // Direct boot needs these out of the image.
        assert!(yaml.contains(KERNEL_NAME));
        assert!(yaml.contains(INITRD_NAME));
        assert!(yaml.contains(BASE_META_NAME));
        assert!(yaml.contains("/proc/cmdline"), "the cmdline must be captured, not invented");
    }

    #[test]
    fn login_profile_flushes_before_cutting_power() {
        // The project lives on 9p; a forced poweroff without sync can lose writes.
        // Match the commands, not the comment that mentions them.
        let sync = LOGIN_PROFILE.find("\n        sync\n").expect("no sync before poweroff");
        let off = LOGIN_PROFILE.find("sudo poweroff -f").expect("not a forced poweroff");
        assert!(sync < off, "sync must run before power is cut");
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

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn ssh_credentials_collects_default_keys_and_config() {
        let temp = std::env::temp_dir().join(format!("geli_ssh_test_{}", std::process::id()));
        let ssh_dir = temp.join(".ssh");
        fs::create_dir_all(&ssh_dir).unwrap();

        fs::write(ssh_dir.join("id_ed25519"), "test_private_key").unwrap();
        fs::write(ssh_dir.join("id_ed25519.pub"), "test_public_key").unwrap();
        fs::write(ssh_dir.join("known_hosts"), "github.com ssh-ed25519 AAAA...").unwrap();

        let creds = collect_ssh_credentials(&temp, None).unwrap();
        assert!(creds.iter().any(|(p, c)| p == ".ssh/id_ed25519" && c == "test_private_key"));
        assert!(creds.iter().any(|(p, c)| p == ".ssh/id_ed25519.pub" && c == "test_public_key"));
        assert!(creds.iter().any(|(p, c)| p == ".ssh/known_hosts" && c.contains("github.com")));

        let config = |creds: &[(String, String)]| {
            creds.iter().find(|(p, _)| p == ".ssh/config").map(|(_, c)| c.clone()).unwrap()
        };

        // known_hosts came across, so there is something to check against and checking is strict.
        let strict = config(&creds);
        assert!(strict.contains("StrictHostKeyChecking yes"), "{}", strict);
        assert!(strict.contains("IdentityFile ~/.ssh/id_ed25519"));

        // Without it the alternative is a session hanging on a prompt nobody will answer, so
        // first contact is accepted — but only then, not as a blanket default.
        fs::remove_file(ssh_dir.join("known_hosts")).unwrap();
        let lenient = config(&collect_ssh_credentials(&temp, None).unwrap());
        assert!(lenient.contains("StrictHostKeyChecking accept-new"), "{}", lenient);

        // An empty known_hosts is no better than a missing one.
        fs::write(ssh_dir.join("known_hosts"), "\n  \n").unwrap();
        let empty = config(&collect_ssh_credentials(&temp, None).unwrap());
        assert!(empty.contains("StrictHostKeyChecking accept-new"), "{}", empty);

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn ssh_credentials_collects_custom_key() {
        let temp = std::env::temp_dir().join(format!("geli_custom_key_test_{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();
        let key_file = temp.join("custom_deploy_key");
        fs::write(&key_file, "custom_secret").unwrap();

        let creds = collect_ssh_credentials(&temp, Some(&key_file)).unwrap();
        assert!(creds.iter().any(|(p, c)| p == ".ssh/custom_deploy_key" && c == "custom_secret"));

        let config_entry = creds.iter().find(|(p, _)| p == ".ssh/config").map(|(_, c)| c.as_str()).unwrap();
        assert!(config_entry.contains("IdentityFile ~/.ssh/custom_deploy_key"));

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn cli_parses_ssh_flags() {
        use clap::Parser;
        let args = Cli::parse_from(["geli", "--ssh", "claude"]);
        assert!(args.ssh);
        assert_eq!(args.ssh_key, None);
        assert_eq!(args.agent_args, vec!["claude"]);

        let args_key = Cli::parse_from(["geli", "--ssh-key", "/tmp/my_key", "opencode"]);
        assert!(!args_key.ssh);
        assert_eq!(args_key.ssh_key, Some(PathBuf::from("/tmp/my_key")));
        assert_eq!(args_key.agent_args, vec!["opencode"]);
    }

    #[test]
    fn save_local_ssh_updates_local_config() {
        let temp = std::env::temp_dir().join(format!("geli_save_ssh_test_{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();

        // When no config exists yet
        save_local_ssh(&temp, "test_ws", true).unwrap();
        assert!(read_local_ssh(&temp));

        // When config exists with allowlist
        let config_path = temp.join(LOCAL_CONFIG_FILE);
        let cfg: LocalConfig = serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(cfg.workspace, "test_ws");
        assert!(cfg.ssh);

        // Disabling ssh
        save_local_ssh(&temp, "test_ws", false).unwrap();
        assert!(!read_local_ssh(&temp));

        let _ = fs::remove_dir_all(&temp);
    }

    /// Antigravity asks four onboarding questions, a theme, a retention warning and a trust
    /// prompt per folder, each in a different file. A disposable VM gets asked all of it again
    /// every session unless these are seeded.
    #[test]
    fn antigravity_seed_answers_every_first_run_prompt() {
        let agent = agent_for_command("agy").unwrap();
        let seed = seed_files(agent, &["my-repo".to_string()], "");
        let file = |p: &str| {
            seed.iter().find(|(path, _)| path == p).map(|(_, c)| c.clone())
                .unwrap_or_else(|| panic!("{} missing from the seed", p))
        };

        let jetski = file(".gemini/antigravity-cli/jetski_state.pbtxt");
        assert!(jetski.contains("AGENT_ONBOARDING_STATE_COMPLETED"));
        assert!(jetski.contains("POST_ONBOARDING_STEP_TYPE_"));

        let settings = file(".gemini/settings.json");
        assert!(settings.contains("\"theme\": \"Ayu\""));
        assert!(settings.contains("\"warningAcknowledged\": true"));
        assert!(settings.contains("\"onboardingComplete\": true"));

        let cli = file(".gemini/antigravity-cli/settings.json");
        assert!(cli.contains("/workspace/my-repo"));
        assert!(cli.contains("tokyo night"));

        let trusted = file(".gemini/trustedFolders.json");
        assert!(trusted.contains("/workspace/my-repo"));
        assert!(trusted.contains("TRUST_FOLDER"));

        // Every seeded document must be valid on its own; a malformed one is a first-run prompt
        // that comes back, which looks like geli not working rather than a broken file.
        for (path, body) in &seed {
            if path.ends_with(".json") {
                assert!(
                    serde_json::from_str::<serde_json::Value>(body).is_ok(),
                    "{} is not valid JSON:\n{}",
                    path,
                    body
                );
            }
        }
    }

    /// The seed is built from the mounted workspace and nothing else. An earlier version merged
    /// the host's own copies, which carried `~/.gemini/trustedFolders.json` whole — 29 absolute
    /// paths naming other clients' projects, on the machine where this was found — into a VM with
    /// network access. The sandbox has no business learning what else exists on the host.
    #[test]
    fn seeding_never_reads_the_host() {
        let home = std::env::temp_dir().join(format!("geli-seed-host-{}", std::process::id()));
        fs::create_dir_all(home.join(".gemini")).unwrap();
        fs::write(
            home.join(".gemini/trustedFolders.json"),
            r#"{"/home/u/other-client/secret-project": "TRUST_FOLDER"}"#,
        )
        .unwrap();

        for agent in agents() {
            for (path, body) in seed_files(agent, &["mine".to_string()], "sk-ant-key") {
                assert!(
                    !body.contains("other-client") && !body.contains("secret-project"),
                    "{} for {} leaked a host path",
                    path,
                    agent.command
                );
            }
        }

        let _ = fs::remove_dir_all(&home);
    }

    /// `credentials` means "copied out of the user's home", and the status block says so. A path
    /// that geli synthesises must not be declared there, or the recipe misleads whoever reads it
    /// to audit what leaves their machine.
    #[test]
    fn recipes_do_not_claim_credentials_that_are_really_seeded() {
        for agent in agents() {
            let seeded: Vec<String> =
                seed_files(agent, &["w".to_string()], "").into_iter().map(|(p, _)| p).collect();
            for declared in &agent.credentials {
                assert!(
                    !seeded.contains(declared),
                    "{} declares {} as a credential, but geli builds it",
                    agent.command,
                    declared
                );
            }
        }
    }
}
