//! The terminal agents geli knows how to host.
//!
//! A recipe is a file in `agents/`, not code: adding an agent is one TOML file and no Rust. They
//! are parsed once at startup, and a malformed one is a hard error — a half-understood recipe
//! would install the wrong thing or copy the wrong files out of the user's home.

use serde::Deserialize;
use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/recipes.rs"));

/// A terminal coding agent the sandbox knows how to host.
#[derive(Debug, Deserialize)]
pub(crate) struct Agent {
    /// The command as typed. Matched against the first word of the user's command.
    pub(crate) command: String,
    /// Must exist for a golden build to be publishable.
    pub(crate) binary: String,
    /// Shown on the status line.
    pub(crate) label: String,
    /// Paths under `$HOME`, copied verbatim into the guest when *this* agent is invoked.
    ///
    /// Credentials only. Antigravity keeps 1.8 KB of token next to 2.3 GB of conversation
    /// history and a 1.3 GB index in the same directory; the same restraint applies to all of
    /// them. `sensitive_credentials` exists because a recipe is now a file anyone can send.
    pub(crate) credentials: Vec<String>,
    /// Hosts added to the egress allowlist when this agent is invoked.
    pub(crate) hosts: Vec<String>,
    /// Shell that installs it in the golden image. Runs as root, with `retry` in scope.
    pub(crate) install: String,
}

/// Home-directory paths no agent should be asking for. A recipe is a file a stranger can send,
/// and `credentials` is "copy these out of the user's home into a VM with network access".
const NEVER_COPY: &[&str] = &[
    ".ssh", ".aws", ".gnupg", ".kube", ".docker", ".netrc", ".git-credentials", ".config/gh",
    ".pgpass", ".my.cnf",
];

pub(crate) fn agents() -> &'static [Agent] {
    static PARSED: OnceLock<Vec<Agent>> = OnceLock::new();
    PARSED.get_or_init(|| {
        let mut parsed: Vec<Agent> = RECIPES
            .iter()
            .map(|raw| toml::from_str(raw).unwrap_or_else(|e| panic!("bad agent recipe: {}", e)))
            .collect();
        parsed.sort_by(|a, b| a.command.cmp(&b.command));
        parsed
    })
}

/// The agent a command invokes, if geli knows it. Matches the first word however it is pathed.
pub(crate) fn agent_for_command(command: &str) -> Option<&'static Agent> {
    let first = command.split_whitespace().next()?;
    let name = first.rsplit('/').next().unwrap_or(first);
    agents().iter().find(|a| a.command == name)
}

/// Said once when the command is not one of the agents geli knows.
///
/// Not a refusal: opening a shell in the sandbox to try something is legitimate and useful. But
/// the consequences are worth stating, because they are invisible otherwise — no credentials
/// travel, and `--restrict-net` has no agent hosts to open.
pub(crate) fn non_agent_notice(command: &str) -> Option<String> {
    if command.trim().is_empty() || agent_for_command(command).is_some() {
        return None;
    }
    let first = command.split_whitespace().next().unwrap_or(command);
    Some(format!(
        "[·] `{}` is not one of geli's agents ({}). It will run, but no credentials travel\n\
         \x20   into the sandbox and --restrict-net opens no agent hosts.",
        first.rsplit('/').next().unwrap_or(first),
        agents().iter().map(|a| a.command.as_str()).collect::<Vec<_>>().join(", ")
    ))
}

/// Warning shown *before* booting, so a doomed run costs a few seconds rather than a full boot
/// followed by a console that sits there silently.
///
/// geli deliberately does not forward the host's `~/.claude` OAuth credentials into the sandbox,
/// so an agent with no API key reaches its first-run login flow and waits for input that never
/// arrives — which looks exactly like a hang.
pub(crate) fn credential_warning(command: &str, has_credentials: bool) -> Option<String> {
    if has_credentials {
        return None;
    }

    match agent_for_command(command) {
        Some(agent) => Some(format!(
            "[!] No credentials found for `{}`.\n\
             \x20   geli looked for {} and an API key in the environment, and found neither.\n\
             \x20   The agent will stop at its first-run login flow inside the VM and wait for\n\
             \x20   input that never arrives — the session will look like it has hung.",
            agent.command,
            agent
                .credentials
                .iter()
                .map(|p| format!("~/{}", p))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        None => Some(
            "[!] No API credentials in the environment; the sandbox will have none.".to_string(),
        ),
    }
}

/// Credential paths a recipe declares that it has no business declaring.
///
/// Returned rather than rejected: geli tells the user what a recipe wants and lets them decide,
/// because a blanket refusal would also break the legitimate odd case.
pub(crate) fn sensitive_credentials(agent: &Agent) -> Vec<&str> {
    agent
        .credentials
        .iter()
        .filter(|path| {
            NEVER_COPY.iter().any(|bad| {
                let p = path.as_str();
                p == *bad || p.starts_with(&format!("{}/", bad))
            })
        })
        .map(String::as_str)
        .collect()
}
