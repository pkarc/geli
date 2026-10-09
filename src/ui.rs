//! What the user sees while a session runs.
//!
//! Kept apart from the things it describes: the status block and the loading line are the only
//! places geli speaks, and they belong to the terminal, not to the guest or the network.

#[allow(unused_imports)]
use crate::{agents::*, guest::*, net::*, Packages};

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
    tools: &str,
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
    // Directly under `net` on purpose: under --restrict-net these packages are the one thing the
    // session fetches from outside the allowlist, and the two lines have to be read together.
    if !tools.is_empty() {
        out.push_str(&format!("  tools  {}\n", tools));
    }
    if !image.is_empty() {
        out.push_str(&format!("  image  {}\n", image));
    }
    out
}

/// The `tools` line: what `.geli.json` asked to install, named rather than counted.
///
/// **Under `--restrict-net` this line is a disclosure, not a convenience.** The packages are
/// fetched before the lockdown runs, because afterwards there is no route, no resolver and no
/// rule for a package mirror. So a restricted session does reach `dl-cdn.alpinelinux.org` or the
/// npm registry, once, before the agent starts — and the rule in this project is that the status
/// block does not get to leave something like that unsaid.
pub(crate) fn describe_packages(packages: &Packages, restricted: bool) -> String {
    if packages.is_empty() {
        return String::new();
    }

    let groups: Vec<String> = [("apk", &packages.apk), ("npm", &packages.npm), ("pip", &packages.pip)]
        .into_iter()
        .filter(|(_, names)| !names.is_empty())
        .map(|(manager, names)| format!("{} {}", manager, names.join(" ")))
        .collect();

    let listed = groups.join(" · ");
    if restricted {
        format!("{} · fetched before the egress lockdown", listed)
    } else {
        listed
    }
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
