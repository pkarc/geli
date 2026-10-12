//! Egress policy: the allowlist, the CONNECT proxy that enforces it, and what the guest is
//! told so its traffic goes through it.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[allow(unused_imports)]
use crate::{agents::*, guest::*, qemu::*, ui::*};

// --- EGRESS POLICY ---
//
// With `--restrict-net` the guest's only way out is a CONNECT proxy running inside geli, which
// resolves names on the host and only connects to destinations on the allowlist. Three things
// hold that in place, and all three run as root in `mounts.sh` before the agent starts:
//
//   1. the default route is deleted, so nothing off-link is reachable by name or by IP;
//   2. an nftables ruleset drops all egress except TCP to the proxy's port, which is what closes
//      what is still *on-link* — slirp's DNS resolver above all;
//   3. the agent's sudoers is narrowed to `/sbin/poweroff`, so it cannot undo 1 or 2.
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
/// instead, which leaves the on-link 10.0.2.0/24 reachable — the proxy, and everything else slirp
/// puts on that subnet, which is why the route deletion is not the whole policy.
pub(crate) const GUEST_PROXY_HOST: &str = "10.0.2.2";

/// slirp's built-in DNS resolver. On-link, so deleting the default route leaves it answering:
/// measured, `nslookup example.com 10.0.2.3` returned real addresses in a `--restrict-net`
/// session. Names are low bandwidth but they are a channel, and this is the address the ruleset
/// exists to cut off.
pub(crate) const GUEST_DNS: &str = "10.0.2.3";

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
pub(crate) fn build_lockdown(proxy_port: Option<u16>) -> String {
    // No proxy means no `--restrict-net`: the two cannot disagree, because the proxy is the only
    // way out that the policy leaves open. The port itself is not needed here — it is baked into
    // the ruleset file this script applies.
    if proxy_port.is_none() {
        return String::new();
    }

    format!(
        "\n# --- restricted egress ---\n\
         ip route del default || true\n\
         \n\
         # The ruleset is what closes the on-link paths the route deletion leaves open. It is\n\
         # applied atomically from a file, and a failure to apply it is fatal on purpose: the\n\
         # status block has already told the user egress is restricted, and running the agent\n\
         # anyway would make that a lie. Better a session that dies with a reason in the log.\n\
         if ! nft -f /etc/geli/egress.nft; then\n\
         \x20   echo 'geli: FAILED to apply the egress ruleset; refusing to run unrestricted'\n\
         \x20   sync\n\
         \x20   poweroff -f\n\
         fi\n\
         \n\
         # Belt and braces, and it also makes failure fast: with the resolver gone, a lookup\n\
         # fails at once instead of waiting out a timeout on a dropped packet. The ruleset is\n\
         # what makes it binding — this file alone an agent could work around by querying\n\
         # {dns} directly, which is exactly what the measurement showed.\n\
         : > /etc/resolv.conf\n\
         \n\
         # Last, so the agent cannot undo any of the above. Everything from here on is as the\n\
         # sandbox user, with sudo narrowed to turning the machine off.\n\
         printf 'sandbox ALL=(ALL) NOPASSWD: /sbin/poweroff\\n' > /etc/sudoers.d/sandbox\n\
         chmod 0440 /etc/sudoers.d/sandbox\n",
        dns = GUEST_DNS,
    )
}

/// The nftables ruleset for a restricted session, as a guest file rather than a Rust string.
///
/// **The agent relay gets its own rule, not a widened one.** Under `--restrict-net` the guest
/// reaches no SSH server at all, so a forwarded agent buys it nothing it could use against a
/// remote — it is there for the local half, signing commits. The port is still named explicitly
/// rather than opening the host alias, for the same reason the proxy's is: a port left open on
/// `@PROXY_HOST@` is a tunnel out through whatever happens to be listening there.
pub(crate) fn build_egress_ruleset(port: u16, agent_port: Option<u16>) -> String {
    let agent_rule = match agent_port {
        Some(p) => format!(
            "\n        # The ssh-agent relay inside geli. Signing only: the relay refuses every\n\
             \x20       # request that is not a listing or a signature, so this port is not a\n\
             \x20       # second way to reach the host.\n\
             \x20       ip daddr {} tcp dport {} accept\n",
            GUEST_PROXY_HOST, p
        ),
        None => String::new(),
    };
    recipe(
        include_str!("guest/egress.nft"),
        &[
            ("@PROXY_HOST@", GUEST_PROXY_HOST),
            ("@PROXY_PORT@", &port.to_string()),
            ("@DNS@", GUEST_DNS),
            ("@AGENT_RULE@", &agent_rule),
        ],
    )
}

/// `write_files` entry carrying the ruleset, or nothing at all when egress is unrestricted.
///
/// Optional in the same way the credentials entry is: an unrestricted session should not have a
/// policy file sitting in it at all, so there is nothing to wonder about when reading the guest.
pub(crate) fn build_egress_entry(proxy_port: Option<u16>, agent_port: Option<u16>) -> String {
    let Some(port) = proxy_port else {
        return String::new();
    };
    format!(
        "  - path: /etc/geli/egress.nft\n    \
         permissions: '0600'\n    \
         content: |\n{}\n",
        indent_block(&build_egress_ruleset(port, agent_port), 6)
    )
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
pub(crate) fn refuse(client: &mut TcpStream, status: &str) {
    let _ = client.write_all(
        format!("HTTP/1.1 {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", status)
            .as_bytes(),
    );
    let _ = client.flush();
    let _ = client.shutdown(std::net::Shutdown::Both);
}

pub(crate) fn note(log: &Mutex<File>, line: &str) {
    if let Ok(mut f) = log.lock() {
        let _ = writeln!(f, "{}", line);
    }
}

pub(crate) fn serve(mut client: TcpStream, allow: &[String], log: &Mutex<File>, stats: &Mutex<ProxyStats>) {
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

pub(crate) fn deny(client: &mut TcpStream, stats: &Mutex<ProxyStats>, host: &str) {
    if let Ok(mut s) = stats.lock() {
        s.blocked += 1;
        s.blocked_hosts.insert(host.to_string());
    }
    refuse(client, "403 Forbidden");
}

pub(crate) fn tunnel(client: TcpStream, upstream: TcpStream) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;


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
    /// The relay gets a rule of its own, named explicitly, and only when there is a relay.
    #[test]
    fn the_agent_relay_gets_its_own_rule_or_none() {
        let without = build_egress_ruleset(45678, None);
        assert_eq!(without.matches("accept").count(), 2, "loopback and the proxy, nothing else");

        let with = build_egress_ruleset(45678, Some(4242));
        assert!(with.contains("tcp dport 4242 accept"));
        assert!(with.contains("tcp dport 45678 accept"));
        assert_eq!(with.matches("accept").count(), 3);
        // Named, not widened: the host alias must never be opened as a whole.
        assert!(!with.contains("ip daddr 10.0.2.2 accept"));
        assert!(!with.contains("@AGENT_RULE@"), "the placeholder survived into the ruleset");
    }

    #[test]
    fn egress_ruleset_opens_only_the_proxy_port() {
        let rules = build_egress_ruleset(45678, None);
        assert!(rules.contains("policy drop"));
        assert!(rules.contains("ip daddr 10.0.2.2 tcp dport 45678 accept"));
        assert!(rules.contains(r#"oifname "lo" accept"#), "the agent's own loopback must survive");

        // The resolver is the reason this file exists; it must not appear as an accept rule.
        for line in rules.lines().filter(|l| l.trim_start().starts_with("ip daddr")) {
            assert!(!line.contains(GUEST_DNS), "the resolver must not be reachable: {}", line);
        }
        // `inet`, not `ip`: a v4-only table would leave IPv6 egress wide open.
        assert!(rules.contains("table inet geli"));

        assert_eq!(build_egress_entry(None, None), "", "an open session gets no policy file at all");
        assert!(build_egress_entry(Some(45678), None).contains("/etc/geli/egress.nft"));
    }

    /// The lockdown has to be in place before the agent runs, and the readiness marker is what
    /// tells the host the guest is ready — so the lockdown must come first.
    #[test]
    fn lockdown_runs_before_the_session_is_declared_ready() {
        let plan = build_mount_script(
            &[PathBuf::from("/home/u/proj")],
            Path::new("/home/u/proj"),
            "",
            "",
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
}
