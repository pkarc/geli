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
    /// Shell that installs it into its own qcow2 layer. Runs as root, with `retry` in scope.
    pub(crate) install: String,
    /// Shell that prints the installed version, recorded in the layer's `.meta` and shown in the
    /// status block. Optional: most agents answer `--version` with one usable line.
    #[serde(default)]
    pub(crate) version: Option<String>,
}

impl Agent {
    pub(crate) fn version_command(&self) -> String {
        match &self.version {
            Some(cmd) => cmd.clone(),
            None => format!("{} --version", self.binary),
        }
    }
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

/// The agent a command invokes, if geli knows it. Matches the invoked program however it is
/// pathed.
///
/// **Takes the program, not the whole command line.** It used to take the line and split off the
/// first word, which stopped working the moment the line started being shell-quoted: `'claude'`
/// matches no recipe, and the session would have run with no credentials and no complaint.
/// `GuestCommand` keeps the two forms apart so the quoted one cannot arrive here.
pub(crate) fn agent_for_command(program: &str) -> Option<&'static Agent> {
    let name = program.rsplit('/').next().unwrap_or(program);
    agents().iter().find(|a| a.command == name)
}

/// Said once when the command is not one of the agents geli knows.
///
/// Not a refusal: opening a shell in the sandbox to try something is legitimate and useful. But
/// the consequences are worth stating, because they are invisible otherwise — no credentials
/// travel, and `--restrict-net` has no agent hosts to open.
pub(crate) fn non_agent_notice(program: &str) -> Option<String> {
    if program.trim().is_empty() || agent_for_command(program).is_some() {
        return None;
    }
    Some(format!(
        "[·] `{}` is not one of geli's agents ({}). It will run, but no credentials travel\n\
         \x20   into the sandbox and --restrict-net opens no agent hosts.",
        program.rsplit('/').next().unwrap_or(program),
        agents().iter().map(|a| a.command.as_str()).collect::<Vec<_>>().join(", ")
    ))
}

/// Warning shown *before* booting, so a doomed run costs a few seconds rather than a full boot
/// followed by a console that sits there silently.
///
/// geli deliberately does not forward the host's `~/.claude` OAuth credentials into the sandbox,
/// so an agent with no API key reaches its first-run login flow and waits for input that never
/// arrives — which looks exactly like a hang.
pub(crate) fn credential_warning(program: &str, has_credentials: bool) -> Option<String> {
    if has_credentials {
        return None;
    }

    match agent_for_command(program) {
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

/// Everything geli says about this session before the VM boots, in the order it says it.
///
/// **Gathered into one pure function so what gets said is testable without booting anything.**
/// These were four `eprintln!`s spread through `execute_sandbox`, which meant the only way to
/// check that a `--restrict-net --ssh` run warns about SSH was to do it and watch. Each of them
/// exists because its absence cost someone real time, so they are worth a test apiece.
pub(crate) fn session_warnings(
    agent: Option<&Agent>,
    program: &str,
    has_credentials: bool,
    forward_ssh: bool,
    restrict_net: bool,
) -> Vec<String> {
    let mut out = Vec::new();

    // Not "may be blocked": the egress ruleset is default-drop with a single rule for the proxy's
    // port, so nothing reaches port 22, and git over SSH does not read the proxy variables the way
    // curl and npm do. Saying "may" would send someone hunting for a configuration that does not
    // exist. The keys are still worth forwarding here — SSH commit signing is entirely local.
    if forward_ssh && restrict_net {
        out.push(
            "[!] --restrict-net blocks SSH outright: egress is default-drop except the proxy's\n\
             \x20   port, and git over SSH ignores the proxy variables. Pushing and fetching over\n\
             \x20   ssh:// will fail. The forwarded keys still work for local use, such as\n\
             \x20   signing commits. Use an https:// remote to push under --restrict-net.\n"
                .to_string(),
        );
    }

    if let Some(agent) = agent {
        let risky = sensitive_credentials(agent);
        if !risky.is_empty() {
            out.push(format!(
                "\n[!] The `{}` recipe asks to copy {} out of your home.\n\
                 \x20   Those are not agent credentials. Check agents/{}.toml before trusting it,\n\
                 \x20   or run with --no-credentials.\n",
                agent.command,
                risky.join(", "),
                agent.command
            ));
        }
    }

    out.extend(non_agent_notice(program));
    out.extend(credential_warning(program, has_credentials).map(|w| format!("\n{}\n", w)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each of these exists because its absence cost someone real time. The order matters too:
    /// the session-shaping warnings come before the ones about the command itself.
    #[test]
    fn a_session_says_what_is_unusual_about_it() {
        let claude = agent_for_command("claude");

        // The ordinary case is silent. Anything geli prints every run is noise.
        assert!(session_warnings(claude, "claude", true, false, false).is_empty());

        // --ssh under --restrict-net: the keys go in and SSH still cannot reach anything.
        let both = session_warnings(claude, "claude", true, true, true);
        assert_eq!(both.len(), 1);
        assert!(both[0].contains("blocks SSH outright"));
        assert!(both[0].contains("https://"), "it must name the way out, not just the problem");
        assert!(session_warnings(claude, "claude", true, true, false).is_empty());

        // An unknown command and no credentials: both said, notice before the warning.
        let bare = session_warnings(None, "bash", false, false, false);
        assert_eq!(bare.len(), 2);
        assert!(bare[0].contains("not one of geli's agents"));
        assert!(bare[1].contains("No API credentials"));
    }

    /// The warning has to name the agent and the paths, or it is a reason to ignore it.
    #[test]
    fn a_recipe_reaching_outside_its_own_credentials_is_named() {
        let reaching = Agent {
            command: "greedy".to_string(),
            binary: "greedy".to_string(),
            label: "greedy".to_string(),
            credentials: vec![".ssh/id_ed25519".to_string()],
            hosts: Vec::new(),
            install: String::new(),
            version: None,
        };

        let said = session_warnings(Some(&reaching), "greedy", true, false, false);
        assert!(said.iter().any(|w| w.contains(".ssh/id_ed25519")), "{:?}", said);
        assert!(said.iter().any(|w| w.contains("agents/greedy.toml")));
        assert!(said.iter().any(|w| w.contains("--no-credentials")));
    }
}
