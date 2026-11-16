//! Hashes the components shipped with this build (`components/dist`, filled by
//! `mise run components`) into the table that approves them without hashes in their entry.

use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::Path;

fn sha256(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Sha256::digest(&bytes)
        .iter()
        .fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn main() {
    let root = std::env::var("CARGO_MANIFEST_DIR").expect("set by Cargo");
    let dist = Path::new(&root).join("../../components/dist");
    // A directory is scanned for changes; a missing one is checked again at every build.
    println!("cargo::rerun-if-changed={}", dist.display());
    let mut wasm: Vec<_> = match std::fs::read_dir(&dist) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "wasm"))
            .collect(),
        Err(_) => {
            println!(
                "cargo::warning=components/dist is missing: this build approves no component by \
                 itself (run `mise run components` before building)"
            );
            Vec::new()
        }
    };
    wasm.sort();
    let mut table = String::from("&[\n");
    for path in wasm {
        let manifest = path.with_extension("toml");
        if !manifest.exists() {
            println!(
                "cargo::warning={} has no manifest beside it: not shipped",
                path.display()
            );
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("UTF-8 name");
        writeln!(
            table,
            "    ({name:?}, {:?}, {:?}),",
            sha256(&path),
            sha256(&manifest)
        )
        .expect("writing to a String");
    }
    table.push(']');
    let out = std::env::var("OUT_DIR").expect("set by Cargo");
    std::fs::write(Path::new(&out).join("shipped.rs"), table).expect("cannot write shipped.rs");
}
