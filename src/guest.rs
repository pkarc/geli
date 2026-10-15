//! What the guest is told to be: recipes, cloud-init, mounts, and the boot it reports back.

#[allow(unused_imports)]
use crate::{agents::*, net::*, qemu::*, ui::*, Packages};
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

/// The retry helper, substituted into the base recipe, every agent layer and the mount script of
/// a session that declared `packages`, so they cannot drift. An agent's `install` field is
/// documented as running with `retry` in scope.
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

/// Marker the guest writes to the boot console for each package a session added. The host already
/// polls that log for readiness, so reusing it keeps this to zero extra devices.
///
/// **A session can write to that console, so this is a proposal and not a fact.** `/dev/ttyS1` is
/// made world-writable for the boot breadcrumb, which means an agent can print these lines
/// itself. That is why the host validates every name it reads and why nothing is written to
/// `.geli.json` without the user saying yes.
pub(crate) const INSTALLED_MARKER: &str = "geli:installed";

/// Where the baseline snapshot lives in the guest, taken once the declared packages are in.
pub(crate) const PACKAGE_BASELINE: &str = "/etc/geli/packages.base";

/// The guest-side helper that takes the snapshot and reports the difference. Baked into the base
/// image because it is entirely static — the rule that keeps session boots at seconds.
pub(crate) fn packages_helper() -> String {
    recipe(
        include_str!("guest/packages.sh"),
        &[("@CONSOLE@", BOOT_CONSOLE.split(',').next().unwrap_or("ttyS1"))],
    )
}

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

/// The user's command, in the two forms geli needs it.
///
/// **They are not the same string, and conflating them is a silent failure.** `line` is quoted,
/// because the guest's shell re-splits whatever it is handed and `geli claude -p "explain this"`
/// used to arrive as four arguments. `program` is the raw first word, because that is what
/// `agent_for_command` matches against a recipe — and a quoted `'claude'` matches nothing, which
/// would not break the session, it would run it with no credentials and say nothing.
pub(crate) struct GuestCommand {
    /// The invoked program as the user typed it, however it is pathed. Never quoted.
    pub(crate) program: String,
    /// Every argument quoted and joined: what the guest's shell is given to run.
    pub(crate) line: String,
}

pub(crate) fn build_guest_command(argv: &[String]) -> GuestCommand {
    GuestCommand {
        program: argv.first().cloned().unwrap_or_default(),
        line: argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "),
    }
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

/// Where the guest's ssh clients look for the agent.
pub(crate) const AGENT_SOCKET: &str = "/tmp/geli-agent.sock";

/// Where the user's credentials wait until the risky part of the boot is over.
///
/// **No `owner:` on what cloud-init writes here, which is the point.** `write_files` runs before
/// `runcmd`, so anything it puts in `/home/sandbox` is readable by the first thing that runs —
/// including a package's install script. Staged root-only and installed afterwards, those files
/// exist for a shorter time and for fewer readers.
pub(crate) const SECRETS_DIR: &str = "/etc/geli/secrets";

/// The user half of the CONNECT credentials. Carries nothing: the token is the secret, and this
/// only exists because `Proxy-Authorization: Basic` is shaped `user:pass`.
pub(crate) const AGENT_PROXY_USER: &str = "geli";

/// The guest half of the agent relay: a Unix socket that forwards to geli's listener on the host.
///
/// **A relay is needed because 9p cannot carry a Unix socket.** Measured: the guest can `bind()`
/// one on a 9p share and the host does see the inode — `srwxr-xr-x` — but connecting to it from
/// the host gives `ECONNREFUSED`. The inode crosses, the connection does not. So the socket is
/// local to the guest and `socat` carries each connection to `10.0.2.2:<port>`, the same slirp
/// host alias the egress proxy uses.
///
/// Owned by `sandbox`, mode 0600: the agent runs as that user, and nothing else in the guest has
/// any business asking the host to sign.
///
/// **The session's token rides in a CONNECT, because `socat` can send one and cannot prepend
/// bytes.** geli's listener is TCP on the host's loopback, and a TCP listener has no permissions:
/// every local user of the host can reach it, where the real agent socket sits behind a `0700`
/// directory. So the relay wants a secret before it will talk.
///
/// Getting that secret in front of the stream is the whole design question, and it was measured
/// the hard way. A shell dialer — `{ printf TOKEN; cat; } | socat - TCP:…` — looks obvious and
/// does not work: with the shell's builtin `printf` the token sits in the subshell's buffer until
/// the connection closes, and with an external one the token leaves at once but the *request*
/// behind it does not. Both failures look identical from the guest: `ssh-add` hangs.
///
/// `PROXY:` sidesteps all of it. socat writes `CONNECT` with `Proxy-Authorization` itself, in one
/// process, before any data — and the host already knows this shape, because the egress proxy is
/// the same conversation. → `docs/plans/10-paquetes-token-y-lo-que-no-se-puede-comprobar.md`
pub(crate) fn build_agent_relay(relay: Option<(u16, &str)>) -> String {
    let Some((port, token)) = relay else {
        return String::new();
    };
    format!(
        "\n# --- ssh agent relay ---\n\
         rm -f {socket}\n\
         setsid socat UNIX-LISTEN:{socket},fork,mode=0600,user=sandbox \\\n\
         \x20   PROXY:{host}:agent:1,proxyport={port},proxyauth={user}:{token} \\\n\
         \x20   >/dev/null 2>&1 &\n",
        socket = AGENT_SOCKET,
        host = crate::net::GUEST_PROXY_HOST,
        port = port,
        user = AGENT_PROXY_USER,
        token = token,
    )
}

/// The order of this script is the session's security model in one place.
///
/// **Caches, packages, workspace, credentials, relay, lockdown — and each step is where it is for
/// a reason that cost something to learn:**
///
/// - Packages go in while there is still a route. The lockdown deletes the default route, empties
///   `resolv.conf` and loads a default-drop ruleset, and a package fetched after it fails
///   *quietly*, since a missing one only warns.
/// - **The workspace is mounted after them**, so a package's install script — which runs as root
///   here, and which no approval can vouch for across versions — has no project to read. This was
///   impossible until a package name stopped being allowed to be a path.
/// - **The credentials are installed after that**, out of `/etc/geli/secrets`, for the same
///   reason: `write_files` runs before `runcmd`, so anything left in the home by cloud-init is
///   already there when the first install script runs.
/// - The relay must be listening before the agent could ask it to sign, and before the ruleset
///   that names its port.
///
/// → `docs/plans/11-el-orden-dentro-del-arranque.md`
pub(crate) fn build_mount_script(
    dirs: &[PathBuf],
    current_canonical: &Path,
    packages: &str,
    agent_relay: &str,
    lockdown: &str,
) -> MountPlan {
    let mut script = String::from("#!/bin/bash\nset -x\n\nmkdir -p /workspace\n");
    let mut active_folder = String::new();
    let mut folders = Vec::new();

    let mut workspace_mounts = String::new();
    for (i, dir) in dirs.iter().enumerate() {
        let tag = share_tag(i);
        let name = folder_name(dir);
        folders.push(name.clone());

        if is_active_dir(dir, current_canonical) {
            active_folder = name.clone();
        }

        workspace_mounts.push_str(&format!(
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

    // apk looks for its cache at /etc/apk/cache, which Alpine ships as nothing at all; the
    // conventional target is /var/cache/apk, which the base recipe empties on its way out. The
    // symlink is recreated rather than assumed, so a session never silently downloads twice.
    script.push_str("mkdir -p /var/cache/apk\n");
    script.push_str(&format!(
        "mount -t 9p -o {MOUNT_OPTS} apkcache /var/cache/apk\n"
    ));
    script.push_str("rm -rf /etc/apk/cache\nln -s /var/cache/apk /etc/apk/cache\n");

    // Declared tools, with a route to fetch them over, no project to read and no credentials on
    // disk. The two lines that put those there come after. `cd /` because npm and pip read a
    // name ending in .tgz or .whl as a file in the working directory, and a protection that
    // depends on where a script happens to be standing is not one.
    script.push_str("\ncd /\n");
    script.push_str(packages);

    // The baseline for "what did this session add", taken *after* the declared packages are in:
    // otherwise declaring one would make it show up as a discovery on every later run, and the
    // prompt at the end would never go quiet. Guarded on the helper existing, because a base
    // image built before it does not carry it.
    script.push_str(&format!(
        "\ncommand -v geli-packages >/dev/null && geli-packages snapshot > {PACKAGE_BASELINE}\n"
    ));

    // Now the project, and only now.
    script.push('\n');
    script.push_str(&workspace_mounts);

    // And the user's credentials, staged by cloud-init where only root could read them.
    script.push_str(&format!(
        "\nif [ -d {SECRETS_DIR} ]; then\n\
         \x20 (cd {SECRETS_DIR} && find . -type f -exec install -D -m 0600 \\\n\
         \x20     -o sandbox -g sandbox {{}} /home/sandbox/{{}} \\;)\n\
         \x20 rm -rf {SECRETS_DIR}\n\
         fi\n"
    ));

    // Before the lockdown, like the packages: the relay has to be listening before the agent
    // could possibly ask it for a signature, and the ruleset that comes next names its port.
    script.push_str(agent_relay);

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

/// The shell that installs a project's declared tools, spliced into `build_mount_script` ahead of
/// the lockdown.
///
/// **Declared, not accumulated.** Keeping the root disposable means anything the agent installs by
/// hand is gone next session; the price of getting it back is one line in `.geli.json`, and what
/// it buys is a result that survives a layer rebuild, reads in a diff and explains itself. See
/// `docs/history-management.md` §5 and appendix C.
///
/// Three things here are not cosmetic:
///
/// - **Each install runs as root but points at the sandbox user's cache.** The npm and pip caches
///   arrive on 9p under `/home/sandbox/.cache`, and root's own cache is elsewhere — without the
///   explicit flags every boot would re-download what is already on the host's disk, which is the
///   whole reason those shares exist.
/// - **A failure warns and continues.** The contrast is with the egress ruleset, which powers the
///   VM off: there the status block has already claimed the session is restricted and running on
///   would make that a lie. Here nothing has been claimed — the agent is short a tool and will
///   find out when it reaches for it.
/// - **Names go through `shell_quote` as well as `valid_package_name`.** The validator already
///   rejects everything that means something to a shell, so the quoting is redundant today; it
///   stays because the day someone widens that charset, the quoting is what keeps this from
///   becoming an injection.
pub(crate) fn build_package_setup(packages: &Packages) -> String {
    if packages.is_empty() {
        return String::new();
    }

    let mut script = String::from("\n");
    script.push_str(GUEST_RETRY.trim_end());
    script.push('\n');

    // apk first: the other two are only as reliable as the toolchain underneath them, and a
    // project that declares a build dependency expects it present before npm reaches for it.
    for (label, command, names) in [
        ("apk", "apk add --no-progress", &packages.apk),
        (
            "npm",
            "npm install -g --cache /home/sandbox/.cache/npm --prefer-offline",
            &packages.npm,
        ),
        (
            "pip",
            // Alpine marks its python externally managed, so a bare `pip install` refuses.
            "pip install --break-system-packages --cache-dir /home/sandbox/.cache/pip",
            &packages.pip,
        ),
    ] {
        if names.is_empty() {
            continue;
        }
        let quoted: Vec<String> = names.iter().map(|n| shell_quote(n)).collect();
        script.push_str(&format!(
            "retry {} {} || echo \"geli: {} install failed; the session continues without it\"\n",
            command,
            quoted.join(" "),
            label
        ));
    }

    script
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
                "  - path: {SECRETS_DIR}/{}\n    \
                 permissions: '0600'\n    \
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
            ("@PACKAGES@", &indent_block(&packages_helper(), 6)),
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
    agent_port: Option<u16>,
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
            ("@EGRESS@", &build_egress_entry(proxy_port, agent_port)),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use yaml_rust2::YamlLoader;

    fn session_cloud_init(dirs: &[&str], current: &str, cmd: &str) -> String {
        session_cloud_init_with(dirs, current, cmd, &Packages::default())
    }

    fn session_cloud_init_with(
        dirs: &[&str],
        current: &str,
        cmd: &str,
        packages: &Packages,
    ) -> String {
        let dirs: Vec<PathBuf> = dirs.iter().map(PathBuf::from).collect();
        let plan =
            build_mount_script(&dirs, Path::new(current), &build_package_setup(packages), "", "");
        let env = build_env_exports(&[("ANTHROPIC_API_KEY", "sk-test".to_string())]);
        let seed = vec![(".claude.json".to_string(), build_claude_config(&plan.folders, "sk-ant-test-key-0123456789"))];
        build_cloud_init(&plan, cmd, &env, &[], &seed, None, None)
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

        // The declared tools go into the same literal block as the mounts, and that block is
        // where the document's shape has broken before: the install lines carry quotes, braces
        // and `|`, and the retry helper brings its own blank lines.
        let yaml = session_cloud_init_with(
            &["/home/u/proj"],
            "/home/u/proj",
            "echo hello",
            &Packages {
                apk: vec!["ripgrep".into()],
                npm: vec!["@scope/pkg".into()],
                pip: vec!["httpx".into()],
            },
        );
        let parsed = YamlLoader::load_from_str(&yaml);
        assert!(parsed.is_ok(), "packages broke the document: {:?}\n---\n{}", parsed.err(), yaml);
        assert!(yaml.contains("'ripgrep'"), "the install never reached the document");
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
        let plan = build_mount_script(&dirs, Path::new("/home/u/proj"), "", "", &build_lockdown(Some(45678)));
        let seed = vec![(".claude.json".to_string(), build_claude_config(&plan.folders, "sk-ant-test"))];
        let creds = vec![(".claude/.credentials.json".to_string(), "{\"t\":1}".to_string())];
        let yaml = build_cloud_init(&plan, "claude", "", &creds, &seed, Some(45678), None);

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

    /// **The order of the boot is the security model, so it is asserted as an order.**
    ///
    /// A package's install script runs as root with a route out, and an approval covers names,
    /// not what a version does later. What it finds when it runs is decided here: no project
    /// mounted yet, and no credentials on disk yet.
    #[test]
    fn nothing_a_package_could_read_exists_while_it_installs() {
        let plan = build_mount_script(
            &[PathBuf::from("/home/u/proj")],
            Path::new("/home/u/proj"),
            &build_package_setup(&Packages {
                apk: vec!["ripgrep".into()],
                ..Default::default()
            }),
            &build_agent_relay(Some((4242, "cafed00d"))),
            &crate::net::build_lockdown(Some(45678)),
        );
        let at = |needle: &str| plan.script.find(needle).unwrap_or_else(|| panic!("no {}", needle));

        let caches = at("npmcache");
        let packages = at("apk add");
        let workspace = at("projshare1");
        let secrets = at(SECRETS_DIR);
        let lockdown = at("ip route del default");
        let ready = at(READY_MARKER);

        assert!(caches < packages, "the caches are what make installing worth doing");
        assert!(packages < workspace, "a package install script could read the project");
        assert!(packages < secrets, "a package install script could read the credentials");
        assert!(secrets < lockdown, "the agent would start without its credentials");
        assert!(lockdown < ready, "the session was declared ready before egress was cut");

        // npm and pip read a name ending in .tgz or .whl as a file in the working directory.
        assert!(plan.script[..packages].contains("\ncd /\n"), "installing from an unknown cwd");
    }

    /// The relay has to be listening before the agent could ask it for a signature, and before
    /// the lockdown, whose ruleset names its port.
    #[test]
    fn the_agent_relay_starts_before_egress_is_cut() {
        let plan = build_mount_script(
            &[PathBuf::from("/home/u/proj")],
            Path::new("/home/u/proj"),
            "",
            &build_agent_relay(Some((4242, "cafed00d"))),
            &crate::net::build_lockdown(Some(45678)),
        );

        let relay = plan.script.find("socat").expect("no relay in the script");
        let cut = plan.script.find("ip route del default").expect("no lockdown");
        let ready = plan.script.find(READY_MARKER).expect("no ready marker");
        assert!(relay < cut, "the relay must be up before the route goes");
        assert!(cut < ready, "the session must not be declared ready before the lockdown");

        // Owned by the user the agent runs as, and reachable at the slirp host alias.
        assert!(plan.script.contains("user=sandbox"));
        assert!(plan.script.contains("mode=0600"));
        assert!(plan.script.contains("proxyport=4242"));

        // The token goes in the CONNECT socat sends, because socat can send one and cannot
        // prepend bytes to a stream. It keeps other users of the *host* out of a TCP port that
        // has no permissions; the guest is the intended client and has the socket already.
        assert!(
            plan.script.contains("PROXY:10.0.2.2:agent:1"),
            "the relay must ask through a CONNECT, not dial the port raw"
        );
        assert!(plan.script.contains("proxyauth=geli:cafed00d"), "the CONNECT carries no token");
    }

    /// A session without `--ssh` must not have a relay, a socket, or a line about one.
    #[test]
    fn no_agent_means_no_relay_at_all() {
        assert_eq!(build_agent_relay(None), "");
        let plan = build_mount_script(
            &[PathBuf::from("/home/u/proj")],
            Path::new("/home/u/proj"),
            "",
            &build_agent_relay(None),
            "",
        );
        assert!(!plan.script.contains("socat"));
        assert!(!plan.script.contains(AGENT_SOCKET));
    }

    #[test]
    fn build_mount_script_marks_current_directory_as_active() {
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        let dirs = vec![tmp.clone()];
        let plan = build_mount_script(&dirs, &tmp, "", "", "");
        assert_eq!(plan.active_folder, folder_name(&tmp));
    }

    #[test]
    fn build_mount_script_falls_back_when_no_directory_matches() {
        // Nothing canonicalizes to this path, so the fallback in build_mount_script applies.
        let plan = build_mount_script(&[], Path::new("/home/u/myproject"), "", "", "");
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

    /// `geli claude -p "explain this module"` used to reach the guest as four arguments, because
    /// the line was joined with spaces and the guest's shell split it again.
    #[test]
    fn arguments_keep_their_quoting() {
        let argv: Vec<String> =
            ["claude", "-p", "explain this module"].iter().map(|s| s.to_string()).collect();
        assert_eq!(build_guest_command(&argv).line, r"'claude' '-p' 'explain this module'");

        let awkward: Vec<String> =
            ["sh", "-c", "echo $HOME && it's fine"].iter().map(|s| s.to_string()).collect();
        assert_eq!(build_guest_command(&awkward).line, r"'sh' '-c' 'echo $HOME && it'\''s fine'");

        assert_eq!(build_guest_command(&[]).line, "");
    }

    /// The quoted line and the matched program are deliberately two fields. Matching a recipe
    /// against `'claude'` finds nothing — and that failure is silent in the worst direction: the
    /// session runs, with no credentials, saying only that the command is not an agent.
    #[test]
    fn the_agent_is_matched_before_the_command_is_quoted() {
        let argv: Vec<String> =
            ["/usr/local/bin/claude", "--resume"].iter().map(|s| s.to_string()).collect();
        let cmd = build_guest_command(&argv);

        assert_eq!(agent_for_command(&cmd.program).map(|a| a.command.as_str()), Some("claude"));
        assert!(agent_for_command(&cmd.line).is_none(), "the quoted line must never be matched");
        assert!(non_agent_notice(&cmd.program).is_none());
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
        let plan = build_mount_script(&dirs, Path::new("/home/u/proj"), "", "", "");
        let env = format!(
            "{}\n{}",
            build_env_exports(&[("ANTHROPIC_API_KEY", "sk-test".to_string())]),
            build_terminal_setup("screen-256color", "truecolor", Some((24, 100)))
        );
        let yaml = build_cloud_init(&plan, "claude", &env, &[], &[], None, None);

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
        let plan = build_mount_script(&dirs, Path::new("/home/u/proj"), "", "", "");
        let creds = vec![(
            ".claude/.credentials.json".to_string(),
            r#"{"claudeAiOauth":{"accessToken":"tok","refreshToken":"ref"}}"#.to_string(),
        )];
        let yaml = build_cloud_init(&plan, "claude", "", &creds, &[], None, None);

        assert!(YamlLoader::load_from_str(&yaml).is_ok(), "{}", yaml);
        assert!(yaml.contains("accessToken"));

        // **Staged, not placed.** cloud-init writes before anything in `runcmd` runs, so a
        // credential it drops straight into the home is readable by the first package install
        // script of the boot. It waits in a root-only directory and the mount script installs it
        // once the risky part is over.
        let staged = format!("{}/.claude/.credentials.json", SECRETS_DIR);
        assert!(yaml.contains(&staged), "the credential was not staged: {}", yaml);
        assert!(
            !yaml.contains("path: /home/sandbox/.claude/.credentials.json"),
            "cloud-init still writes the credential into the home"
        );

        let block = yaml.split(&staged).nth(1).unwrap();
        assert!(block.contains("permissions: '0600'"));
        // No owner either: `sandbox` must not be able to read it while it is waiting.
        let entry = block.split("- path:").next().unwrap();
        assert!(!entry.contains("owner: sandbox"), "the staged credential is the agent's early");

        // And the mount script installs it, after the packages and before the lockdown.
        let install = plan.script.find("/etc/geli/secrets").expect("nothing installs the secrets");
        assert!(plan.script[install..].contains("0600"));
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
        let packages = Packages {
            apk: vec!["ripgrep".into(), "jq".into()],
            npm: vec!["@scope/pkg".into()],
            pip: vec!["httpx".into()],
        };
        let plan = build_mount_script(
            &dirs,
            Path::new("/home/u/proj"),
            &build_package_setup(&packages),
            "",
            &build_lockdown(Some(45678)),
        );
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
            ("session.cloud-init.yaml", build_cloud_init(&plan, "claude", &env, &creds, &seed, Some(45678), None)),
            ("setup.sh", base_setup_script(1000)),
            ("verify.sh", base_verify_script()),
            ("profile.sh", LOGIN_PROFILE.to_string()),
            ("autologin.sh", AUTOLOGIN_HELPER.to_string()),
            ("packages.sh", packages_helper()),
            ("mounts.sh", plan.script.clone()),
            ("claude.json", seed[0].1.clone()),
            ("lockdown.sh", build_lockdown(Some(45678))),
            ("egress.nft", build_egress_ruleset(45678, None)),
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

    fn sample_packages() -> Packages {
        Packages {
            apk: vec!["ripgrep".into(), "jq".into()],
            npm: vec!["prettier".into()],
            pip: Vec::new(),
        }
    }

    /// The order is the whole correctness of this feature. The lockdown deletes the default
    /// route, empties `/etc/resolv.conf` and loads a default-drop ruleset, so a package fetched
    /// after it cannot succeed — and because a failed install only warns, it would not succeed
    /// *quietly*: the session would start without its tools and the reason would be one line in a
    /// console log nobody reads.
    #[test]
    fn packages_install_before_egress_is_cut() {
        let plan = build_mount_script(
            &[PathBuf::from("/home/u/proj")],
            Path::new("/home/u/proj"),
            &build_package_setup(&sample_packages()),
            "",
            &build_lockdown(Some(45678)),
        );

        let apk = plan.script.find("apk add").expect("no apk install");
        let cut = plan.script.find("ip route del default").expect("no lockdown");
        let ready = plan.script.find(READY_MARKER).expect("no ready marker");
        assert!(apk < cut, "packages are fetched after egress was cut, so they cannot be fetched");
        assert!(cut < ready, "the lockdown must still precede the marker");

        // The caches only help if the install actually points at them: these run as root, whose
        // own npm and pip caches are somewhere else entirely.
        assert!(plan.script.contains("mount -t 9p"), "no 9p mounts at all");
        assert!(plan.script.contains("apkcache /var/cache/apk"));
        assert!(plan.script.contains("--cache /home/sandbox/.cache/npm"));

        // A missing tool is not a broken promise, unlike a ruleset that failed to load — so the
        // install block warns where the lockdown powers off. Asserted on the block alone: the
        // lockdown's own `poweroff` lives further down the same script.
        let block = build_package_setup(&sample_packages());
        assert!(block.contains("install failed"), "a failed install says nothing");
        assert!(!block.contains("poweroff"), "a failed install must not kill the session");
    }

    /// Nothing declared, nothing emitted: a session that asked for no tools must generate the
    /// same script it did before this existed.
    #[test]
    fn no_packages_means_no_install_step() {
        assert_eq!(build_package_setup(&Packages::default()), "");
    }

    /// The baseline has to be taken after the declared packages are installed, or every run would
    /// re-offer what `.geli.json` already says.
    #[test]
    fn the_baseline_is_taken_after_the_declared_installs() {
        let plan = build_mount_script(
            &[PathBuf::from("/home/u/proj")],
            Path::new("/home/u/proj"),
            &build_package_setup(&sample_packages()),
            "",
            &build_lockdown(Some(45678)),
        );

        let install = plan.script.find("apk add").expect("no install");
        let baseline = plan.script.find("geli-packages snapshot").expect("no baseline");
        assert!(install < baseline, "the baseline would record the declared packages as absent");

        // And the report is the other half, from the profile that runs after the command.
        assert!(LOGIN_PROFILE.contains("geli-packages report"));
        let report = LOGIN_PROFILE.find("geli-packages report").unwrap();
        let off = LOGIN_PROFILE.find("sudo poweroff -f").unwrap();
        assert!(report < off, "the VM would be gone before it said what it installed");
    }

    /// Regression guard for the point of this change: the session must leave the guest's console
    /// somewhere other than the terminal, and signal readiness through it.
    #[test]
    fn mount_script_ends_with_the_ready_marker() {
        let plan = build_mount_script(&[PathBuf::from("/home/u/proj")], Path::new("/home/u/proj"), "", "", "");
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
}
