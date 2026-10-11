# Why

`CLAUDE.md` holds the rules — what to do, what not to do, and the one clause that stops you doing
it wrong. These notes hold how each was arrived at: the measurement, or the sequence of failures
that produced it.

The split exists because the two have different readers. A rule has to be short enough to be read
every time; the reasoning only has to be there when someone is about to change it. Most of these
rules look arbitrary until you know what they cost, which is why every one of them links here.

| | |
|---|---|
| [`build-and-ci.md`](build-and-ci.md) | The two workflows, why `cargo fmt` is not a gate, where code and tests live |
| [`images-and-boot.md`](images-and-boot.md) | The image chain, the build lock, direct boot, the memory ceiling |
| [`guest-config.md`](guest-config.md) | The three cloud-init documents, the YAML's fixed shape, login and the tty |
| [`output.md`](output.md) | stderr vs stdout, the status block, the loading line |
| [`agents-and-credentials.md`](agents-and-credentials.md) | Recipes as data, credentials vs seeded config, SSH keys |
| [`egress.md`](egress.md) | `--restrict-net`: the proxy, the ruleset, the lockdown's order |
| [`workspaces-and-session.md`](workspaces-and-session.md) | The registry, the project file, and a session's own state |

Decisions that were taken and then *rejected* live in [`../plans/`](../plans/) instead, with the
measurements that killed them.
