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

## SSH: the agent is forwarded, the key is not

Off by default. With `--ssh` the guest gets a socket at `/tmp/geli-agent.sock`, `SSH_AUTH_SOCK`
points at it, and `socat` carries each connection to a relay thread inside geli at
`10.0.2.2:<port>` — the same slirp host alias the egress proxy uses. The host's `ssh-agent` does
the signing. `known_hosts` and a generated `~/.ssh/config` still cross; those describe the hosts
being talked to, not the user.

**A relay is required because 9p cannot carry a Unix socket.** Measured: the guest *can* `bind()`
one on a 9p share and the host *does* see the inode —

```
srwxr-xr-x 1 pkarc pkarc 0 Oct 11 19:18 .geli-xsock
```

— but connecting to it from the host gives `ECONNREFUSED`. The inode crosses, the connection does
not. Mounting the host's socket and being done with it does not work in either direction.

**vsock was the better shape and was rejected on permissions.** `-device vhost-vsock-pci` would
give a host↔guest channel with no network in it, invisible to the egress policy. But on an
ordinary Ubuntu desktop `/dev/kvm` is `crw-rw----+` — udev puts an ACL on it for the session
user — and `/dev/vhost-vsock` is `crw-rw----` with no ACL and no membership of `kvm`. Opening it
gives `EACCES`. The device exists and does not work, which is the lesson `/dev/kvm` already
taught; the difference is that there the answer was to degrade to emulation, and here there is no
degradation — either the channel exists or the session cannot start.

### The relay is a filter, and that is the point

Forwarding an agent wholesale lets the far end enumerate every key you have loaded and delete them
from your agent. `screen` passes only `REQUEST_IDENTITIES` and `SIGN_REQUEST`; add, remove,
remove-all, lock, unlock, smartcard and constrained-add are answered with `SSH_AGENT_FAILURE`
without the host's agent ever hearing them. Verified against a real agent: `ssh-add -D` inside the
VM reports "Failed to remove all identities" and the host still holds its three keys.

`--ssh-key` restricts further. The blob in a `.pub` file is the one the agent reports for that key,
so `filter_identities` rewrites the listing to hold only it and sign requests for anything else are
refused. Without that, `--ssh-key` would name one key and hand over all of them. The listing is
rewritten rather than refused because `ssh` enumerates before it signs; refusing there would simply
stop it finding the key it is allowed to use.

**Refusing and reporting are two different things**, and conflating them was measured to be wrong.
The first end-to-end run printed a refusal on every ordinary `ssh` connection, because ssh opens a
forwarded-agent connection with `session-bind@openssh.com` and carries on happily when the agent
says no. A warning that fires when nothing is wrong is one people learn to scroll past — and the
one it would hide is the guest reshaping the user's agent. Extensions are refused silently;
everything else is refused out loud.

### What it does not give

For as long as the session runs, the guest can ask for a signature over anything it can reach. It
cannot steal the key, but it can act as you. This is the classic weakness of agent forwarding, and
the recommendation has always been not to forward an agent to a machine you do not trust — which
is literally geli's use case.

So the claim is not "now it is safe". It is:

> *the key stays yours forever* instead of *the key may end up someone else's*

Permanent exposure becomes exposure bounded by the session. `ssh-add -c` is the cheap mitigation —
it makes the agent ask before each signature — at the cost of any non-interactive use.

### The flags

**`--ssh-copy-key` is the old behaviour under a name that says what it does.** It copies
`~/.ssh/id_*` in, and it warns every time, because a key in a VM outlives the VM. The status block
distinguishes the two in words that cannot be mistaken for each other:
`ssh-agent forwarded · key stays on the host` against `SSH private key COPIED into the VM`.

**No agent means a refusal, never a quiet fallback.** `host_agent_socket` fails with the command to
fix it. Falling to the weaker mode without saying so would leave the user unable to tell which one
they got, which is the shape of failure the rest of this project exists to avoid.

`.ssh` stays in `NEVER_COPY`, so a *recipe* can never ask for any of this quietly, and the standing
grant lives in the registry rather than the project — see
[`workspaces-and-session.md`](workspaces-and-session.md).

**Host-key checking is strict when `known_hosts` came across, `accept-new` only when it did not** —
and that case prints a line saying so. Blanket `accept-new` trusts whatever answers first, which is
a poor default in the one environment where you least control what is running.

**`--restrict-net` still blocks SSH outright**, and the warning must say so: the ruleset is
default-drop and git over SSH does not read the proxy variables that curl and npm do. The agent
remains useful there for the local half — signing commits — so the relay gets **its own nft rule**
rather than a widened one, named explicitly like the proxy's. Measured in a restricted session:
`ssh-add -l` lists the keys, `ssh git@github.com` cannot even resolve, and `ssh-keygen -Y sign`
reaches the signing step.
