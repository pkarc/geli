# Why — the egress policy (`--restrict-net`)

Off by default; with the flag the guest reaches only `DEFAULT_ALLOWED_HOSTS` plus whatever the
workspace's `~/.config/geli/workspaces/<name>.json` lists under `allow`. **Not the project's
`.geli.json`** — a file the sandbox can write must not be able to widen its own egress.

A CONNECT proxy runs on a thread inside geli, resolves on the host, and only dials port 443 — a
proxy that reaches any port on an allowed host is a general tunnel, not a policy.

## Do not reach for `restrict=on` + `guestfwd`

Measured on QEMU 8.2.2: a guestfwd forwards exactly one connection and then times out forever, with
or without `restrict`, so a session died after its first request. The guest reaches the proxy at
slirp's host alias `10.0.2.2:<port>` instead, and egress is cut by deleting the guest's default
route — which leaves the internet unreachable by name *and* by IP while the on-link proxy still
answers.

## Cutting the route is only half of it

The agent is root in the guest by default and can add the route straight back, so restricted mode
also narrows its sudoers to `/sbin/poweroff` alone. Both happen in `mounts.sh`, as root, before the
agent runs — and before the readiness marker, or the agent could start first. A restriction the
sandbox can undo is worse than none, because the status line claims it is on.

## The route deletion is not the whole policy; the ruleset is

Deleting the default route leaves the on-link 10.0.2.0/24 reachable, and slirp puts its DNS resolver
there at `10.0.2.3`. Measured before the fix: `nslookup example.com 10.0.2.3` returned real
addresses in a `--restrict-net` session, so names were a working channel out.

`src/guest/egress.nft` is a default-drop output chain allowing only loopback and TCP to the proxy's
port; the family is `inet`, so it covers IPv6 in the same rules. The guest never needs a resolver of
its own — it speaks CONNECT by name and the proxy resolves on the host — which is what makes
default-drop survivable.

**A ruleset that fails to apply powers the session off.** By that point the status block has told
the user egress is restricted; running the agent anyway would make that a lie. `/etc/resolv.conf` is
also emptied, which is belt-and-braces — an agent could query `10.0.2.3` directly without it — but
it turns a dropped-packet timeout into an instant failure.

## The lockdown's order is load-bearing

Route, then ruleset, then sudo. The agent is root until the last step, so narrowing sudo first would
leave the two steps that matter without the privilege to do their job.

## What this does not give

An allowed destination is still a way out — `github.com` permits a gist. And anything that ignores
the proxy variables now has no network at all rather than a direct path: that is the intended shape,
unreachable rather than unsupervised.
