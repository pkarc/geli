//! What the guest is told to be: recipes, cloud-init, mounts, and the boot it reports back.

#[allow(unused_imports)]
use crate::{agents::*, net::*, qemu::*, ui::*};
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

/// Printed by the build VM only if every expected tool is actually present. cloud-init does not
/// abort runcmd on failure, so a sentinel that is merely "reached" would prove nothing.
pub(crate) const BUILD_OK_MARKER: &str = "GELI_BUILD_OK";

/// Major Node version the agent requires. Alpine's own `nodejs` package satisfies it, so unlike
/// on Ubuntu there is no tarball to fetch — apt's Node 18 was the reason that existed.
pub(crate) const REQUIRED_NODE_MAJOR: u32 = 22;

/// The retry helper, substituted into the base recipe and every agent layer so the two cannot
/// drift. An agent's `install` field is documented as running with `retry` in scope.
pub(crate) const GUEST_RETRY: &str = include_str!("guest/retry.sh");

/// Provisioning run once by `--build-image`: the toolchain, the user, autologin — and no agent.
///
/// `bash` is not optional: Alpine's default shell is busybox ash, and the agent's Bash tool
/// needs real bash. Everything else is the same toolchain the Ubuntu recipe installed.
pub(crate) fn base_setup_script(host_uid: u32) -> String {
    recipe(
        include_str!("guest/setup.sh"),
        &[
            ("@RETRY@", GUEST_RETRY.trim_end()),
            ("@HOST_UID@", &host_uid.to_string()),
            ("@MOUNT_OPTS@", MOUNT_OPTS),
            ("@OUT_TAG@", BUILD_OUT_TAG),
            ("@KERNEL@", KERNEL_NAME),
            ("@INITRD@", INITRD_NAME),
            ("@META@", BASE_META_NAME),
        ],
    )
}

/// Provisioning for one agent's qcow2 layer, run on top of the base image.
///
/// One agent per layer, and one layer per session: a layer that installed two could not be reused
/// by a session wanting only one of them, which is the whole reason layers exist.
pub(crate) fn agent_layer_script(agent: &Agent, host_uid: u32) -> String {
    recipe(
        include_str!("guest/layer.sh"),
        &[
            ("@RETRY@", GUEST_RETRY.trim_end()),
            ("@INSTALL@", agent.install.trim()),
            ("@BINARY@", &agent.binary),
            ("@COMMAND@", &agent.command),
            ("@VERSION@", &agent.version_command()),
            ("@HOST_UID@", &host_uid.to_string()),
            ("@MOUNT_OPTS@", MOUNT_OPTS),
            ("@OUT_TAG@", BUILD_OUT_TAG),
            ("@META@", &layer_meta_name(&agent.command)),
            ("@OK_MARKER@", BUILD_OK_MARKER),
        ],
    )
}

/// 9p tag the build VM uses to hand the kernel, initramfs and metadata back to the host.
pub(crate) const BUILD_OUT_TAG: &str = "geliout";

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

pub(crate) const AUTOLOGIN_HELPER: &str = include_str!("guest/autologin.sh");

/// Printed only when every tool is present *and* Node is new enough. cloud-init does not abort
/// runcmd on failure, so the host greps for this rather than trusting the build "finished".
pub(crate) fn base_verify_script() -> String {
    recipe(
        include_str!("guest/verify.sh"),
        &[
            ("@NODE_MAJOR@", &REQUIRED_NODE_MAJOR.to_string()),
            ("@OK_MARKER@", BUILD_OK_MARKER),
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
pub(crate) const LOGIN_PROFILE: &str = include_str!("guest/profile.sh");

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
/// Config an agent should find already answered, **synthesised by geli rather than copied**.
///
/// The distinction from `credentials` is the whole point of this function existing separately:
/// a credential is read out of the user's home and the status block names it; a seed file is
/// built here from nothing but the mounted workspace and, for Claude, the tail of an API key. So
/// a seed file cannot carry host state the user did not ask for, and does not belong on the list
/// of things that left their home.
///
/// **Nothing here reads the host's copy of these files, deliberately.** An earlier version merged
/// them, which meant Antigravity's `~/.gemini/trustedFolders.json` travelled whole: measured on
/// one developer's machine, 29 absolute paths naming projects for several different clients, into
/// a VM with network access. The agent needs to know this sandbox's folders are trusted; it has
/// no business learning what else exists.
///
/// This is also the one place where per-agent Rust is justified, so it is kept to one `match`
/// rather than scattered. Suppressing a first-run flow means knowing that agent's file formats,
/// and a TOML field expressive enough to describe "merge these keys into this JSON document"
/// would be a programming language with worse ergonomics than this. Everything *else* about an
/// agent stays data in `agents/*.toml`.
pub(crate) fn seed_files(agent: &Agent, folders: &[String], api_key: &str) -> Vec<(String, String)> {
    match agent.command.as_str() {
        "claude" => vec![(".claude.json".to_string(), build_claude_config(folders, api_key))],
        "agy" => antigravity_seed(folders),
        _ => Vec::new(),
    }
}

/// The guest paths a session mounts, which is all a seed file is ever allowed to know.
fn workspace_paths(folders: &[String]) -> Vec<String> {
    folders.iter().map(|f| format!("/workspace/{}", f)).collect()
}

/// Antigravity's first run asks four onboarding questions, a theme, a retention warning and a
/// trust prompt per folder. Each lives in a different file, which is why there are five.
fn antigravity_seed(folders: &[String]) -> Vec<(String, String)> {
    use serde_json::json;

    let paths = workspace_paths(folders);
    let pretty = |v: serde_json::Value| serde_json::to_string_pretty(&v).unwrap_or_default();

    vec![
        (
            ".gemini/antigravity-cli/jetski_state.pbtxt".to_string(),
            "post_onboarding: {\n  \
               completed_steps: POST_ONBOARDING_STEP_TYPE_MANAGER_WELCOME\n  \
               completed_steps: POST_ONBOARDING_STEP_TYPE_USAGE_MODE\n  \
               completed_steps: POST_ONBOARDING_STEP_TYPE_AGENT_CONFIGURATION\n  \
               completed_steps: POST_ONBOARDING_STEP_TYPE_ADD_WORKSPACE\n\
             }\n\
             agent_onboarding_completed: AGENT_ONBOARDING_STATE_COMPLETED\n"
                .to_string(),
        ),
        (
            ".gemini/settings.json".to_string(),
            pretty(json!({
                "onboardingComplete": true,
                "consumerOnboardingComplete": true,
                "enterpriseOnboardingComplete": true,
                "ui": { "theme": "Ayu" },
                "general": {
                    "sessionRetention": { "enabled": true, "warningAcknowledged": true }
                }
            })),
        ),
        (
            ".gemini/antigravity-cli/settings.json".to_string(),
            pretty(json!({
                "onboardingComplete": true,
                "consumerOnboardingComplete": true,
                "enterpriseOnboardingComplete": true,
                "colorScheme": "tokyo night",
                "initialColorScheme": "tokyo night",
                "trustedWorkspaces": paths,
            })),
        ),
        (
            ".gemini/trustedFolders.json".to_string(),
            pretty(serde_json::Value::Object(
                paths.iter().map(|p| (p.clone(), json!("TRUST_FOLDER"))).collect(),
            )),
        ),
        (
            ".gemini/state.json".to_string(),
            pretty(json!({ "tipsShown": 10, "focusUiEnabled": false })),
        ),
    ]
}

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

/// What a build recorded about the image it produced.
///
/// One `.meta` per image: the base records what it provisioned, each agent layer records the
/// version of the one agent it added. A session parses the base's and then every layer in its
/// chain, so `agents` ends up holding exactly what is in the image it booted — no list of agent
/// names appears in this file, which is what keeps a new recipe from needing Rust.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct ImageMeta {
    /// The kernel command line the image boots itself with, captured rather than invented.
    pub(crate) cmdline: String,
    pub(crate) alpine: String,
    pub(crate) node: String,
    /// `(command, version)` for each agent the chain carries, in the order read.
    pub(crate) agents: Vec<(String, String)>,
}

/// Parse one or more `.meta` files, later ones adding to earlier ones.
pub(crate) fn parse_image_meta(raw: &str) -> ImageMeta {
    let mut meta = ImageMeta::default();
    merge_image_meta(&mut meta, raw);
    meta
}

pub(crate) fn merge_image_meta(meta: &mut ImageMeta, raw: &str) {
    for line in raw.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_string();
        match key.trim() {
            "cmdline" => meta.cmdline = value,
            "alpine" => meta.alpine = value,
            "node" => meta.node = value,
            key => {
                if let Some(command) = key.strip_prefix("agent.") {
                    meta.agents.push((command.to_string(), value));
                }
            }
        }
    }
}

pub(crate) fn describe_image(meta: &ImageMeta) -> String {
    let mut parts = Vec::new();
    if !meta.alpine.is_empty() {
        parts.push(format!("alpine {}", meta.alpine));
    }
    for (command, version) in &meta.agents {
        if !version.is_empty() {
            parts.push(format!("{} {}", command, version));
        }
    }
    parts.join(" · ")
}

/// Furthest phase the boot log shows evidence of.
pub(crate) fn boot_phase(log: &str) -> &'static str {
    BOOT_PHASES
        .iter()
        .rev()
        .find(|(_, marker)| log.contains(marker))
        .map(|(name, _)| *name)
        .unwrap_or("starting")
}

/// Rewrite the image's own command line for a direct boot: the guest's console moves off the
/// user's terminal, and the kernel stops narrating.
///
/// `root=` and `modules=` are carried over untouched — they describe how this particular image
/// finds its filesystem, and hardcoding them is the kind of guess that breaks silently. The
/// bootloader's own `BOOT_IMAGE=` and `initrd=` are dropped, since there is no bootloader now.
pub(crate) fn boot_cmdline(original: &str) -> String {
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

/// Guest recipes live in `src/guest/` as real shell and YAML, not as Rust string literals.
///
/// They were literals until the file passed 2,700 lines, and every `{` in a shell script had to
/// be doubled to survive `format!`. Placeholders are `@NAME@` and substituted here, which keeps
/// the files readable — and runnable — on their own.
pub(crate) fn recipe(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (key, value) in values {
        out = out.replace(key, value);
    }
    debug_assert!(
        !out.contains('@') || !out.contains("@\n") || values.is_empty(),
        "a recipe placeholder went unsubstituted"
    );
    out
}

/// cloud-config for `--build-image`. Everything slow and workspace-independent lives here.
///
/// Alpine, not Ubuntu: measured at a third of the disk footprint (632 MB against 1.9 GB) with a
/// newer kernel and far fewer packages — which is the point of a sandbox. The differences from
/// the Ubuntu recipe are apk instead of apt, an inittab line instead of a systemd drop-in, and
/// no Node tarball, since Alpine already ships Node 22.
pub(crate) fn build_base_cloud_init(host_uid: u32) -> String {
    recipe(
        include_str!("guest/base.yaml"),
        &[
            ("@AUTOLOGIN@", &indent_block(AUTOLOGIN_HELPER, 6)),
            ("@PROFILE@", &indent_block(LOGIN_PROFILE, 6)),
            ("@SETUP@", &indent_block(&base_setup_script(host_uid), 6)),
            ("@VERIFY@", &indent_block(&base_verify_script(), 6)),
        ],
    )
}

/// cloud-config that turns a copy-on-write layer into "the base image plus one agent".
pub(crate) fn build_layer_cloud_init(agent: &Agent, host_uid: u32) -> String {
    recipe(
        include_str!("guest/layer.yaml"),
        &[("@LAYER@", &indent_block(&agent_layer_script(agent, host_uid), 6))],
    )
}

/// cloud-config for one sandbox session. Installs nothing: the golden image already has it.
pub(crate) fn build_cloud_init(
    plan: &MountPlan,
    command: &str,
    env_exports: &str,
    // Read out of the user's home. Named in the status block, because they left it.
    credentials: &[(String, String)],
    // Built by geli from the workspace alone — same shape in the guest, different provenance.
    seed: &[(String, String)],
    proxy_port: Option<u16>,
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

    // Both end up as the same kind of write_files entry; only their provenance differs, and that
    // difference is kept in Rust rather than in the guest, which has no use for it.
    let mut written: Vec<(String, String)> = credentials.to_vec();
    written.extend(seed.iter().cloned());

    recipe(
        include_str!("guest/session.yaml"),
        &[
            ("@ENV@", &indent_block(env_exports, 6)),
            ("@MOUNTS@", &indent_block(&plan.script, 6)),
            ("@SESSION@", &indent_block(&session, 6)),
            ("@CREDENTIALS@", &build_credentials_entry(&written)),
            ("@EGRESS@", &build_egress_entry(proxy_port)),
        ],
    )
}
