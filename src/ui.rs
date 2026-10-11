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

/// Which host directory lands where in the guest, and which one the command starts in.
///
/// Pure, so the status block's contents are testable without a VM — it is the only place the
/// user is told what the sandbox can see, and a wrong line there is a wrong belief about what the
/// agent was given.
pub(crate) fn status_mounts(dirs: &[std::path::PathBuf], current: &std::path::Path) -> Vec<StatusMount> {
    dirs.iter()
        .map(|dir| StatusMount {
            host: dir.display().to_string(),
            guest: format!("/workspace/{}", crate::guest::folder_name(dir)),
            active: crate::guest::is_active_dir(dir, current),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Exactly one directory is active — the one the command `cd`s into — and the guest path is
    /// derived from the folder name, not the full host path.
    #[test]
    fn mounts_name_the_guest_path_and_mark_the_active_one() {
        let temp = std::env::temp_dir().join(format!("geli_mounts_{}", std::process::id()));
        let a = temp.join("alpha");
        let b = temp.join("beta");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let current = b.canonicalize().unwrap();
        let mounts = status_mounts(&[a.clone(), b.clone()], &current);

        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].guest, "/workspace/alpha");
        assert_eq!(mounts[1].guest, "/workspace/beta");
        assert!(!mounts[0].active);
        assert!(mounts[1].active, "the directory geli was invoked from must be the active one");

        let _ = std::fs::remove_dir_all(&temp);
    }

    /// A directory that no longer exists cannot be canonicalized, and must not be guessed at.
    #[test]
    fn a_missing_directory_is_never_the_active_one() {
        let gone = PathBuf::from("/tmp/geli-does-not-exist");
        let mounts = status_mounts(std::slice::from_ref(&gone), &gone);
        assert!(!mounts[0].active);
    }
}
