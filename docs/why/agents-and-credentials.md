# Why — recipes, credentials, and seeded config

## A recipe is a file in `agents/`, not code

One TOML per agent declaring how it installs, the binary that proves the build worked, the
credential paths it needs and the hosts to open when egress is restricted. `build.rs` discovers
them, so adding an agent is one file and no Rust — that is the point: this is the contribution
surface.

Recipes are sorted by command before use, so a chain's key and an image's recipe hash do not depend
on the filesystem's directory order. `agent_for_command` matches the invoked program, however it is
pathed, and *only that agent's* credentials travel into the guest. An unrecognised command
(`geli bash`) carries none.

**`seed_files` is the one place per-agent Rust is justified, so it is one `match` rather than
scattered.** Suppressing a first-run flow means knowing that agent's file formats, and a TOML field
expressive enough to say "merge these keys into this JSON document" would be a programming language
with worse ergonomics. Everything else about an agent stays data in `agents/*.toml`. A
`if agent.command == "agy"` branch sitting out in `execute_sandbox` is how this went wrong once
already.

**Antigravity's `agy` is a statically linked Go binary**, so musl never enters into it — unlike
every npm-delivered agent, where it does.

## `credentials` is a privileged field and recipes come from strangers

It means "copy these out of the user's home into a VM with network access". `sensitive_credentials`
flags paths that are plainly not agent credentials — `.ssh`, `.aws`, `.gnupg` and friends — and the
status block names every file that leaves the user's home, every run. Neither is a substitute for
reviewing a recipe, but a recipe asking for an SSH key should never be able to do it quietly.

**Credentials only, never the directory they sit in.** Antigravity keeps 1.8 KB of OAuth token in
`~/.gemini/oauth_creds.json` — next to a 1.3 GB index and 317 MB of conversations in the same tree.
Claude's is the same shape. A test asserts no agent's credential list reaches into history.

## Seeded config is not a credential, and the two must not be mixed

`credentials` in a recipe means "copied verbatim out of the user's home", and the status block names
those files because they left it. `seed_files` builds the other kind: settings, trusted folders and
already-answered onboarding, synthesised from the mounted workspace and nothing else.

**Nothing in the seed reads the host's copy, deliberately** — an earlier version merged them, which
sent Antigravity's `~/.gemini/trustedFolders.json` across whole: 29 absolute paths naming several
different clients' projects, into a VM with network access.
`recipes_do_not_claim_credentials_that_are_really_seeded` fails if a recipe declares a path geli
actually builds, and `seeding_never_reads_the_host` fails if a seed picks up host state.

Seeding is also *not* gated on `--no-credentials`: there is nothing of the user's in it, and an
agent running on a bare `ANTHROPIC_API_KEY` still needs its onboarding answered.

## Claude's credential and config

**The host's `~/.claude/.credentials.json` is copied into the guest by default** (`--no-credentials`
opts out). The reasoning is the stated threat model: the sandbox exists to keep the agent away from
files it was not given, and a credential is not one of those. It is also what makes the agent bill
the user's plan instead of API credits. Only that one file is copied — not the rest of `~/.claude`,
which holds conversation transcripts and prompt history across every project.

**Disposable VMs lose agent state, so `build_claude_config` seeds `/home/sandbox/.claude.json`.**
Without it Claude Code re-runs onboarding every session: approve the API key, then trust the folder,
every time. It records `hasCompletedOnboarding`, `hasTrustDialogAccepted` for every mounted
workspace, and the last 20 characters of the key under `customApiKeyResponses.approved` (never the
whole key).

**The guest config holds three keys, and that is the whole of it.** `hasCompletedOnboarding`,
`customApiKeyResponses` and `projects` are what suppress the first-run prompts — measured: with
credentials present but the file absent, the agent still runs onboarding. Copying further state from
the host's `~/.claude.json` (machine/user ids, migration version, cached account profile) was tried
and reverted: it suppressed no additional prompt and changed no measurable startup time, so it only
moved more of the user's data into the guest.
`claude_config_holds_only_what_suppresses_the_prompts` fails if the set grows again.

**Never export an empty `ANTHROPIC_API_KEY`.** Claude Code treats the variable's presence as taking
precedence over a claude.ai login, so a blank export silently shadows the forwarded credentials and
sends the session to API billing — or to nothing. `build_env_exports` filters empty values for this
reason.

**With no credentials at all, `claude` sits in its first-run login flow forever**, which is why
`credential_warning` runs on the host *before* the VM boots rather than letting the user watch a
dead console.

## SSH keys (`--ssh`, `--ssh-key`)

Off by default. With the flag the host's `~/.ssh/id_*` — or one named key — is copied in, along with
`known_hosts` and a generated `~/.ssh/config`.

**This is a different order of risk from forwarding an agent credential, and the README says so
plainly.** An agent credential leaks the account it belongs to; an SSH private key grants write
access to every repository the user has, and it outlives the disposable VM that leaked it. `.ssh` is
in `NEVER_COPY` precisely so a *recipe* can never ask for this quietly — the flag is a first-party,
explicit decision instead, and the copied paths are named in the status block like any other.

**The standing grant lives in the registry, never in the project.** `--ssh` on an interactive
terminal offers to remember the answer for the workspace, and that goes to
`~/.config/geli/workspaces/<name>.json`. It was `"ssh": true` in the project's `.geli.json` until it
was noticed that the sandbox mounts that file with write access: the agent could grant itself the
host's private keys for the next session, with no flag, and so could a cloned repository carrying
the field. `migrate_local_config` reads that field only to tell the user it is being dropped.
`privileged_fields_are_not_read_from_the_project` is the regression test.

**Host-key checking is strict when `known_hosts` came across, `accept-new` only when it did not** —
and that case prints a line saying so. Blanket `accept-new` trusts whatever answers first, which is
a poor default in the one environment where you least control what is running.

**`--restrict-net` blocks SSH outright, and the warning must say so.** The egress ruleset is
default-drop with a single rule for the proxy's port, and git over SSH does not read the proxy
variables that curl and npm do. An earlier wording said connections "may be blocked unless
connecting via port 443", which sends someone hunting for a configuration that does not exist. The
keys remain useful under `--restrict-net` for local work such as commit signing.

**Still open:** forwarding the *agent socket* rather than the key, so the guest can sign with the
key but never read it. geli already has the shape for it — the CONNECT proxy is a host thread the
guest reaches at `10.0.2.2:<port>` — plus a relay in the guest to present a Unix socket.
