//! Discovers agent recipes so adding one is a single file and no Rust.
//!
//! `include_str!` has to name every file, which would mean a contributor editing Rust to add a
//! recipe. This scans `agents/*.toml` instead and generates the list, so a pull request that
//! teaches geli a new agent touches exactly one file.

use std::path::Path;

fn main() {
    let dir = Path::new("agents");
    println!("cargo:rerun-if-changed=agents");

    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("agents/ is missing")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".toml"))
        .collect();
    // Sorted so the generated order — and therefore the golden image recipe hash — is stable
    // regardless of the filesystem's whims.
    names.sort();

    for name in &names {
        println!("cargo:rerun-if-changed=agents/{}", name);
    }

    let entries: String = names
        .iter()
        .map(|n| {
            format!(
                "    include_str!(\"{}/agents/{}\"),\n",
                std::env::var("CARGO_MANIFEST_DIR").unwrap(),
                n
            )
        })
        .collect();

    let generated = format!(
        "/// Every recipe under `agents/`, discovered at build time by build.rs.\n\
         pub(crate) const RECIPES: &[&str] = &[\n{}];\n",
        entries
    );

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("recipes.rs");
    std::fs::write(out, generated).unwrap();
}
