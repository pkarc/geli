//! Egress policy: the allowlist, the CONNECT proxy, and what the guest is told about it.

#[allow(unused_imports)]
use crate::{agents::*, guest::*};

// --- EGRESS POLICY ---
//
// With `--restrict-net` the guest gets `restrict=on`, which blocks outbound traffic *and* DNS,
// and a single forwarded port to a proxy running inside geli. The proxy resolves names on the
// host and only connects to destinations on the allowlist.
//
// What this buys: an arbitrary server is no longer reachable from the sandbox. What it does not
// buy: an agent with `github.com` allowed can still push to a gist. The allowlist narrows
// exfiltration, it does not close it — see README.

/// Reachable by default when restricted: the registries a coding agent needs to do real work in
/// a repository. The agent's own endpoints come from its `Agent` entry, so a session only opens
/// what the agent it is running actually talks to.
pub(crate) const DEFAULT_ALLOWED_HOSTS: &[&str] = &[
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
pub(crate) const ALLOWED_PORT: u16 = 443;

/// slirp's alias for the host. A guest connection to `10.0.2.2:N` arrives at the host's
/// `127.0.0.1:N`, which is where the proxy listens.
///
/// This is deliberately *not* `restrict=on` plus `guestfwd`. Measured on QEMU 8.2.2: a guestfwd
/// forwards exactly one connection and then times out forever, with or without `restrict`, so a
/// session died after its first request. Egress is cut by removing the guest's default route
/// instead, which leaves only the on-link 10.0.2.0/24 — the proxy — reachable.
pub(crate) const GUEST_PROXY_HOST: &str = "10.0.2.2";

/// Matches a host against the allowlist. A rule starting with `.` or `*.` also matches
/// subdomains, which private registries tend to need.
pub(crate) fn host_allowed(host: &str, allow: &[String]) -> bool {
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
pub(crate) enum ProxyRequest {
    Connect { host: String, port: u16 },
    /// Anything else, kept verbatim so a rejection can be explained rather than just dropped.
    Unsupported(String),
}

pub(crate) fn parse_proxy_request(head: &str) -> ProxyRequest {
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
pub(crate) fn build_lockdown(restricted: bool) -> String {
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
pub(crate) fn build_proxy_env(proxy_port: Option<u16>) -> String {
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
pub(crate) fn session_allowlist(agent: Option<&Agent>, extra: &[String]) -> Vec<String> {
    let mut all: Vec<String> = DEFAULT_ALLOWED_HOSTS.iter().map(|h| h.to_string()).collect();
    let agent_hosts: &[String] = agent.map(|a| a.hosts.as_slice()).unwrap_or(&[]);
    for host in agent_hosts.iter().cloned().chain(extra.iter().cloned()) {
        let host = host.trim();
        if !host.is_empty() && !all.iter().any(|h| h.eq_ignore_ascii_case(host)) {
            all.push(host.to_string());
        }
    }
    all
}

/// What `--build-image` recorded about the image it produced.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct GoldenMeta {
    /// The kernel command line the image boots itself with, captured rather than invented.
    pub(crate) cmdline: String,
    pub(crate) alpine: String,
    pub(crate) node: String,
    pub(crate) claude: String,
    pub(crate) opencode: String,
    pub(crate) agy: String,
}

pub(crate) fn parse_golden_meta(raw: &str) -> GoldenMeta {
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
            "opencode" => meta.opencode = value,
            "agy" => meta.agy = value,
            _ => {}
        }
    }
    meta
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

/// One frame of the loading line. Pure so the colour handling is testable: escape codes in a
/// piped log are noise, and `NO_COLOR` exists.
pub(crate) fn render_spinner_line(frame: char, phase: &str, elapsed: f32, color: bool) -> String {
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

/// One mounted directory, as the status block shows it.
pub(crate) struct StatusMount {
    pub(crate) host: String,
    pub(crate) guest: String,
    pub(crate) active: bool,
}

/// The facts worth printing before handing the terminal over: what is mounted, which credential
/// the agent will use, and what is inside the image. Everything else about a session is identical
/// every time, and identical output is noise even when geli writes it.
pub(crate) fn render_status(
    workspace: &str,
    mounts: &[StatusMount],
    auth: &str,
    copied: &[String],
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
    // Named, not summarised. A recipe is a file anyone can contribute, and `credentials` means
    // "copy these out of the user's home into a VM with network access" — so the user sees
    // exactly which files left, every run, without having to go read the recipe.
    for path in copied {
        out.push_str(&format!("         ~/{}\n", path));
    }
    out.push_str(&format!("  net    {}\n", net));
    if !image.is_empty() {
        out.push_str(&format!("  image  {}\n", image));
    }
    out
}

pub(crate) fn describe_image(meta: &GoldenMeta) -> String {
    [
        ("alpine", &meta.alpine),
        ("claude", &meta.claude),
        ("opencode", &meta.opencode),
        ("agy", &meta.agy),
    ]
    .iter()
    .filter(|(_, v)| !v.is_empty())
    .map(|(k, v)| format!("{} {}", k, v))
    .collect::<Vec<_>>()
    .join(" · ")
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
pub(crate) fn build_golden_cloud_init(host_uid: u32) -> String {
    recipe(
        include_str!("guest/golden.yaml"),
        &[
            ("@AUTOLOGIN@", &indent_block(GOLDEN_AUTOLOGIN, 6)),
            ("@PROFILE@", &indent_block(GOLDEN_PROFILE, 6)),
            ("@SETUP@", &indent_block(&golden_setup_script(host_uid), 6)),
            ("@VERIFY@", &indent_block(&golden_verify_script(), 6)),
        ],
    )
}

/// cloud-config for one sandbox session. Installs nothing: the golden image already has it.
pub(crate) fn build_cloud_init(
    plan: &MountPlan,
    command: &str,
    env_exports: &str,
    claude_config: &str,
    credentials: &[(String, String)],
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

    recipe(
        include_str!("guest/session.yaml"),
        &[
            ("@ENV@", &indent_block(env_exports, 6)),
            ("@MOUNTS@", &indent_block(&plan.script, 6)),
            ("@SESSION@", &indent_block(&session, 6)),
            ("@CLAUDE_CONFIG@", &indent_block(claude_config, 6)),
            ("@CREDENTIALS@", &build_credentials_entry(credentials)),
        ],
    )
}

