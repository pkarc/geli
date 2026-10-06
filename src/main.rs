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
    let log_path = dir.join(BUILD_LOG_NAME);

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

/// Remove every agent layer. Called when the base image is rebuilt, since a layer's backing file
/// is gone at that point.
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
#[cfg(target_os = "linux")]
fn ensure_layer(
    dir: &Path,
    agent: &Agent,
    key: &str,
    parent: &Path,
    parent_hash: &str,
) -> io::Result<(PathBuf, String)> {
    let recipe = build_layer_cloud_init(agent, host_uid(), key);
    let hash = layer_hash(parent_hash, &recipe);

    let target = dir.join(layer_image_name(key));
    let recipe_path = dir.join(layer_recipe_name(key));
    let recorded = fs::read_to_string(&recipe_path).unwrap_or_default();

    if target.exists() && recorded.trim() == hash {
        return Ok((target, hash));
    }

    let pending = dir.join(format!("{}.building", layer_image_name(key)));
    let log_path = dir.join(BUILD_LOG_NAME);
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

/// The image a session should overlay, building whatever is missing.
///
/// The chain is base ← one layer per agent, in sorted order, each keyed by the agents at and
/// below it. A session wanting only `claude` reuses the `claude` layer that a `claude+opencode`
/// chain also sits on, instead of a second copy of the same install.
///
/// Also returns the merged metadata: the base's, plus one entry per layer, which is what the
/// status block prints as the contents of the image.
#[cfg(target_os = "linux")]
fn resolve_chain(dir: &Path, chain: &[&Agent]) -> io::Result<(PathBuf, ImageMeta)> {
    let base = dir.join(BASE_IMAGE_NAME);
    let mut meta = parse_image_meta(&fs::read_to_string(dir.join(BASE_META_NAME)).unwrap_or_default());
    let mut image = base;
    let mut hash = fs::read_to_string(dir.join(BASE_RECIPE_NAME)).unwrap_or_default().trim().to_string();

    let commands: Vec<String> = chain.iter().map(|a| a.command.clone()).collect();
    for (i, agent) in chain.iter().enumerate() {
        let key = layer_key(&commands[..=i]);
        let (layer, layer_hash) = ensure_layer(dir, agent, &key, &image, &hash)?;
        merge_image_meta(&mut meta, &fs::read_to_string(dir.join(layer_meta_name(&key))).unwrap_or_default());
        image = layer;
        hash = layer_hash;
    }

    Ok((image, meta))
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
        resolve_chain(&dir, &[agent])?;
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

#[cfg(target_os = "linux")]
fn execute_sandbox(
    ws: &str,
    cur: &Path,
    dirs: Vec<PathBuf>,
    cmd: &str,
    forward_credentials: bool,
    restrict_net: bool,
) -> io::Result<()> {

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
    let chain: Vec<&Agent> = agent.into_iter().collect();
    let (session_backing, meta) = resolve_chain(&images, &chain)?;

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

    // `agent` and the image chain were resolved above, before anything was overlaid.
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    let openai_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();

    // Only the invoked agent's credentials travel, and only the credential — not the history
    // sitting beside it. Opt out entirely with --no-credentials.
    let credentials: Vec<(String, String)> = match (forward_credentials, agent) {
        (true, Some(agent)) => agent
            .credentials
            .iter()
            .filter_map(|rel| {
                fs::read_to_string(home.join(rel)).ok().map(|c| (rel.to_string(), c))
            })
            .collect(),
        _ => Vec::new(),
    };

    let has_credentials = !anthropic_key.trim().is_empty()
        || !openai_key.trim().is_empty()
        || !credentials.is_empty();

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
    // The proxy has to exist before the guest boots: slirp forwards a port straight to it.
    let proxy = if restrict_net {
        let allow = session_allowlist(agent, &read_local_allowlist(cur));
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
        &credentials,
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

    let auth = match (credentials.is_empty(), agent) {
        (false, Some(a)) => format!("{} · bills your plan", a.label),
        (true, _) if !api_key_for_config.trim().is_empty() => {
            "ANTHROPIC_API_KEY · bills API credits".to_string()
        }
        _ => "none".to_string(),
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
        build_cloud_init(&plan, cmd, &env, &config, &[])
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
        // 1.8 KB of token lives beside 2.3 GB of conversation history and a 1.3 GB index.
        assert!(agy.credentials.iter().all(|p| p.starts_with(".gemini/")));
        for history in ["antigravity-cli", "brain", "conversations", "history"] {
            assert!(
                !agy.credentials.iter().any(|p| p.contains(history)),
                "{} would drag history into the guest",
                history
            );
        }

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
        let yaml = build_cloud_init(&plan, "claude", &env, "{}", &[]);

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
        let yaml = build_cloud_init(&plan, "claude", "", "{}", &creds);

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
        let plan = build_mount_script(&dirs, Path::new("/home/u/proj"), &build_lockdown(true));
        let env = format!(
            "{}\n{}\n{}",
            build_env_exports(&[("ANTHROPIC_API_KEY", "sk-fixed".to_string())]),
            build_proxy_env(Some(45678)),
            build_terminal_setup("xterm-256color", "truecolor", Some((46, 190)))
        );
        let config = build_claude_config(&plan.folders, "sk-ant-fixed-0123456789");
        let creds = vec![(".claude/.credentials.json".to_string(), "{\"t\":1}".to_string())];

        let files: Vec<(&str, String)> = vec![
            ("base.cloud-init.yaml", build_base_cloud_init(1000)),
            ("session.cloud-init.yaml", build_cloud_init(&plan, "claude", &env, &config, &creds)),
            ("setup.sh", base_setup_script(1000)),
            ("verify.sh", base_verify_script()),
            ("profile.sh", LOGIN_PROFILE.to_string()),
            ("autologin.sh", AUTOLOGIN_HELPER.to_string()),
            ("mounts.sh", plan.script.clone()),
            ("claude.json", config.clone()),
            ("lockdown.sh", build_lockdown(true)),
        ];
        // One per agent: a layer recipe is the only place an agent's install reaches the guest.
        let mut files = files;
        for agent in agents() {
            files.push((
                // Leaked into the loop's lifetime on purpose — this is a dump, not a test.
                Box::leak(format!("layer.{}.cloud-init.yaml", agent.command).into_boxed_str()),
                build_layer_cloud_init(agent, 1000, &agent.command),
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

    /// Two agents in either order must name the same chain, or the cache stores a copy per
    /// permutation and the saving the layers exist for disappears.
    #[test]
    fn layer_key_is_the_set_not_the_order() {
        let key = |names: &[&str]| layer_key(&names.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(key(&["claude"]), "claude");
        assert_eq!(key(&["opencode", "claude"]), "claude+opencode");
        assert_eq!(key(&["claude", "opencode"]), key(&["opencode", "claude"]));
        assert_eq!(key(&["claude", "claude"]), "claude");
        assert_eq!(key(&[]), "");
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
        let doc = build_layer_cloud_init(agent, 1000, "claude");

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
        assert!(agent_layer_script(agent, 1000, "claude").contains("rm -f /tmp/.geli-session-active"));
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

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}
