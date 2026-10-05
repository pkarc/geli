//! The terminal agents geli knows how to host.

#[allow(unused_imports)]
use crate::{guest::*, net::*};
// --- AGENTS ---
//
// geli runs whatever command you give it, but it knows a few terminal agents by name: enough to
// install them into the image, carry their credentials in, and open the right hosts when egress
// is restricted.

/// A terminal coding agent the sandbox knows how to host.
pub(crate) struct Agent {
    /// The command as typed. Matched against the first word of the user's command.
    pub(crate) command: &'static str,
    /// Shown on the status line.
    pub(crate) label: &'static str,
    /// Shell that installs it in the golden image. Runs as root, with `retry` in scope.
    pub(crate) install: &'static str,
    /// Must exist for a golden build to be publishable.
    pub(crate) binary: &'static str,
    /// Paths under `$HOME`, copied verbatim into the guest when *this* agent is invoked.
    ///
    /// Credentials only. Antigravity keeps 1.8 KB of token next to 2.3 GB of conversation
    /// history and a 1.3 GB index in the same directory; the same restraint applies to all of
    /// them. See the note on `~/.claude` below.
    pub(crate) credentials: &'static [&'static str],
    /// Hosts added to the egress allowlist when this agent is invoked.
    pub(crate) hosts: &'static [&'static str],
}

pub(crate) const AGENTS: &[Agent] = &[
    Agent {
        command: "claude",
        label: "claude.ai credentials",
        install: "retry npm install -g @anthropic-ai/claude-code",
        binary: "claude",
        credentials: &[".claude/.credentials.json"],
        hosts: &[
            "api.anthropic.com",
            "platform.claude.com",
            "console.anthropic.com",
            // MCP connectors reach for this; without it they silently fail to authorise.
            "mcp-proxy.anthropic.com",
        ],
    },
    Agent {
        command: "opencode",
        label: "opencode credentials",
        // npm pulls every platform variant — glibc, musl and a "baseline" of each, ~180 MB
        // apiece — and the postinstall hardlinks the right one into bin/. Deleting the rest took
        // the package from 728 MB to 187 MB with `opencode --version` still answering.
        install: "retry npm install -g opencode-ai \\\n  && rm -rf /usr/local/lib/node_modules/opencode-ai/node_modules/opencode-linux-x64 \\\n       /usr/local/lib/node_modules/opencode-ai/node_modules/opencode-linux-x64-baseline \\\n       /usr/local/lib/node_modules/opencode-ai/node_modules/opencode-linux-x64-baseline-musl",
        binary: "opencode",
        credentials: &[".local/share/opencode/auth.json"],
        // Model-agnostic: it talks to whichever provider you configured, so only its own
        // endpoint is assumed. Add the provider's host to `allow` in .geli.json.
        hosts: &["opencode.ai", "api.opencode.ai"],
    },
    Agent {
        command: "agy",
        label: "Google account credentials",
        // A static Go binary from Google's installer — musl is irrelevant to it. Installed to
        // /usr/local/bin rather than the installer's ~/.local/bin so every user sees it.
        install: "retry sh -c 'curl -fsSL https://antigravity.google/cli/install.sh -o /tmp/agy.sh' \\
  && HOME=/root bash /tmp/agy.sh \\
  && install -m 0755 /root/.local/bin/agy /usr/local/bin/agy \\
  && rm -f /tmp/agy.sh",
        binary: "agy",
        credentials: &[
            ".gemini/oauth_creds.json",
            ".gemini/google_accounts.json",
            ".gemini/installation_id",
        ],
        hosts: &[
            "antigravity.google",
            "generativelanguage.googleapis.com",
            "oauth2.googleapis.com",
            "accounts.google.com",
            "cloudcode-pa.googleapis.com",
        ],
    },
];

/// The agent a command invokes, if geli knows it. Matches the first word however it is pathed.
pub(crate) fn agent_for_command(command: &str) -> Option<&'static Agent> {
    let first = command.split_whitespace().next()?;
    let name = first.rsplit('/').next().unwrap_or(first);
    AGENTS.iter().find(|a| a.command == name)
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
        AGENTS.iter().map(|a| a.command).collect::<Vec<_>>().join(", ")
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

