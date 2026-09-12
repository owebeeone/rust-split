//! Split-mode regression tests on the toolchain fixture — a god-file shaped
//! like the real-world lib.rs that split mode was first rejected on. The three
//! rejection defects are pinned here:
//!
//! 1. re-exports: the crate's public API (`pub use` lines, public items moved
//!    into parts) must survive as `pub use`, never `pub(crate) use`;
//! 2. the `#[cfg(test)]` gate must travel with the extracted tests module;
//! 3. packing must produce multiple cohesion-grouped files near half the
//!    ceiling, not one file grazing it.
//!
//! The output is also compiled with rustc — as a lib, as a `--test` harness
//! (which is then run), and against a dependent consumer crate — so "the split
//! compiles and preserves the public API" is checked by the compiler, not by
//! string inspection alone.

use std::fs;
use std::path::Path;
use std::process::Command;

use rust_split::{SplitOutput, explode, split_bin};

const MAX_LOC: usize = 500;

fn fixture_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/split")
}

fn fixture_src() -> String {
    fs::read_to_string(fixture_dir().join("toolchain_lib.rs")).unwrap()
}

fn split_fixture() -> SplitOutput {
    let src = fixture_src();
    let exploded = explode(&src).unwrap();
    split_bin(&exploded, MAX_LOC, "lib")
}

/// The generated cohesion parts: every output file that is not the spine, the
/// extracted tests module, or the pre-existing helpers module.
fn part_paths(out: &SplitOutput) -> Vec<&str> {
    out.files
        .iter()
        .map(|f| f.path.as_str())
        .filter(|p| !matches!(*p, "lib.rs" | "tests.rs"))
        .collect()
}

#[test]
fn fixture_explodes_losslessly() {
    // The fixture also guards explode mode: chunks must tile the source
    // byte-exactly and deterministically.
    let src = fixture_src();
    let exploded = explode(&src).unwrap();
    let joined: String = exploded.chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(joined, src, "explode must stay lossless");
    assert_eq!(
        explode(&src).unwrap(),
        exploded,
        "explode must stay deterministic"
    );
}

#[test]
fn public_reexports_and_public_items_stay_pub() {
    let out = split_fixture();
    let root = out.files.iter().find(|f| f.path == "lib.rs").unwrap();

    // The source's own re-export and file-module declaration stay verbatim.
    assert!(
        root.contents.contains("mod helpers;"),
        "spine must keep the source `mod helpers;`:\n{}",
        root.contents
    );
    assert!(
        root.contents.contains("pub use helpers::HelperConfig;"),
        "spine must keep the source `pub use` verbatim:\n{}",
        root.contents
    );

    // The part holding the public `Platform` must be re-exported `pub use` so
    // `<crate>::Platform` keeps resolving for dependents.
    let platform_part = out
        .files
        .iter()
        .find(|f| f.contents.contains("pub struct Platform"))
        .expect("some part holds Platform");
    let module = platform_part.path.trim_end_matches(".rs");
    assert!(
        root.contents.contains(&format!("pub use {module}::*;")),
        "part {} holds public items and must be re-exported pub:\n{}",
        platform_part.path,
        root.contents
    );

    // Every part in this fixture carries public API; nothing may be demoted.
    assert!(
        !root.contents.contains("pub(crate) use "),
        "no re-export in this spine may be pub(crate):\n{}",
        root.contents
    );
}

#[test]
fn cfg_test_gate_travels_with_the_extracted_tests_module() {
    let out = split_fixture();
    let root = out.files.iter().find(|f| f.path == "lib.rs").unwrap();
    assert!(
        root.contents.contains("#[cfg(test)]\nmod tests;"),
        "the tests declaration must keep its gate:\n{}",
        root.contents
    );
    let tests = out.files.iter().find(|f| f.path == "tests.rs").unwrap();
    assert!(tests.contents.contains("use super::*;"));
    assert!(
        !tests.contents.contains("mod tests"),
        "tests.rs must hold the unwrapped body, not a nested `mod tests`:\n{}",
        tests.contents
    );
    assert!(
        !root.contents.contains("use tests::*"),
        "an extracted module is declared, never glob re-exported"
    );
}

#[test]
fn packing_yields_multiple_balanced_files_not_one_near_the_ceiling() {
    let out = split_fixture();
    assert!(out.still_oversized.is_empty(), "{:?}", out.still_oversized);

    for f in &out.files {
        assert!(f.loc < MAX_LOC, "{} is over the ceiling: {}", f.path, f.loc);
    }

    let parts = part_paths(&out);
    assert!(
        parts.len() >= 3,
        "an interconnected ~500-LOC domain must spread over >= 3 cohesive \
         parts, got {parts:?}"
    );
    for f in &out.files {
        if parts.contains(&f.path.as_str()) {
            assert!(
                f.loc <= 260,
                "{} grazes the ceiling ({} LOC) — the ceiling is not a packing \
                 target; parts should sit near half of it",
                f.path,
                f.loc
            );
        }
    }
}

#[test]
fn split_output_compiles_and_the_public_api_serves_a_dependent() {
    let out = split_fixture();
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();

    for f in &out.files {
        let path = dir.join(&f.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, &f.contents).unwrap();
    }
    // helpers.rs is a pre-existing sibling module of the source file, not a
    // split product; the split must leave its `mod helpers;` binding valid.
    fs::copy(fixture_dir().join("helpers.rs"), dir.join("helpers.rs")).unwrap();

    // 1. The split lib compiles.
    let rlib = dir.join("libtoolchain_fixture.rlib");
    run_rustc(
        dir,
        &[
            "--edition",
            "2024",
            "--crate-type",
            "lib",
            "--crate-name",
            "toolchain_fixture",
            "lib.rs",
            "-o",
            rlib.to_str().unwrap(),
        ],
    );

    // 2. The cfg(test) code compiles and the moved tests still pass.
    let harness = dir.join("fixture_tests");
    run_rustc(
        dir,
        &[
            "--edition",
            "2024",
            "--test",
            "--crate-name",
            "toolchain_fixture_tests",
            "lib.rs",
            "-o",
            harness.to_str().unwrap(),
        ],
    );
    let tests = Command::new(&harness).output().unwrap();
    assert!(
        tests.status.success(),
        "moved tests must still pass:\n{}",
        String::from_utf8_lossy(&tests.stdout)
    );

    // 3. A dependent crate still resolves the public API — the original
    // rejection was `<crate>::Platform` no longer resolving downstream.
    let consumer_src = r#"use toolchain_fixture::{
    Constraint, HelperConfig, Platform, RegisteredToolchain, Registry,
    ToolchainRequirement, ToolchainType, resolve,
};

fn main() {
    let mut registry = Registry::new();
    registry.register_toolchain(RegisteredToolchain {
        toolchain_type: ToolchainType::new("//cc:toolchain_type"),
        label: "//cc:gcc".to_owned(),
        target_compatible_with: vec![Constraint::new("os:linux")],
        exec_compatible_with: vec![],
    });
    registry.register_execution_platform(Platform::with(&["os:linux"]));
    let resolution = resolve(
        &registry,
        &[ToolchainRequirement::mandatory("//cc:toolchain_type")],
        &Platform::with(&["os:linux"]),
        &HelperConfig::permissive(),
    )
    .expect("public API resolves");
    assert_eq!(resolution.selected.len(), 1);
}
"#;
    fs::write(dir.join("consumer.rs"), consumer_src).unwrap();
    let consumer = dir.join("consumer");
    run_rustc(
        dir,
        &[
            "--edition",
            "2024",
            "consumer.rs",
            "--extern",
            &format!("toolchain_fixture={}", rlib.to_str().unwrap()),
            "-o",
            consumer.to_str().unwrap(),
        ],
    );
    let status = Command::new(&consumer).status().unwrap();
    assert!(status.success(), "consumer must run against the split lib");
}

/// The nested-module rejection: `rust-split split --module` on a `deep.rs`
/// writes its parts into `deep/`, one module level *and* one directory deeper,
/// so a copied `use super::…`, a copied `use self::…` and an `include_str!`
/// path all have to be re-anchored. rustc is the judge — as a lib, and as a
/// `--test` harness so the extracted `#[cfg(test)] mod tests` (which does NOT
/// change level) is checked too.
#[test]
fn split_module_output_compiles_with_rebased_paths() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    fs::create_dir_all(dir.join("docs")).unwrap();
    fs::write(dir.join("docs/note.md"), "note\n").unwrap();
    fs::write(
        dir.join("lib.rs"),
        "pub mod sibling;\npub mod top;\npub mod deep;\n",
    )
    .unwrap();
    fs::write(
        dir.join("sibling.rs"),
        "pub struct Thing;\n\npub fn alpha() -> i32 {\n    1\n}\n",
    )
    .unwrap();
    fs::write(dir.join("top.rs"), "pub struct Absolute;\n").unwrap();

    let god_file = r#"use super::sibling::Thing;
use super::sibling::alpha;
use self::local::X;
use crate::top::Absolute;

pub mod local {
    pub struct X;
}

pub fn helper() -> i32 {
    alpha()
}

pub fn uses_thing(t: Thing, x: X) -> i32 {
    let _ = (t, x);
    helper()
}

pub fn note() -> &'static str {
    include_str!("docs/note.md")
}

pub fn marker() -> i32 {
    let _: Option<Absolute> = None;
    2
}

#[cfg(test)]
mod tests {
    use super::helper;

    #[test]
    fn smoke() {
        assert_eq!(helper(), 1);
    }
}
"#;
    let exploded = explode(god_file).unwrap();
    let out = rust_split::split_mod(&exploded, MAX_LOC, "deep");
    for f in &out.files {
        let path = dir.join(&f.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, &f.contents).unwrap();
    }
    assert!(
        out.files.iter().any(|f| f.path.starts_with("deep/")),
        "the file module's parts go in deep/: {:?}",
        out.files.iter().map(|f| &f.path).collect::<Vec<_>>()
    );

    run_rustc(
        dir,
        &[
            "--edition",
            "2024",
            "--crate-type",
            "lib",
            "--crate-name",
            "nested_fixture",
            "lib.rs",
            "-o",
            dir.join("libnested_fixture.rlib").to_str().unwrap(),
        ],
    );

    let harness = dir.join("nested_tests");
    run_rustc(
        dir,
        &[
            "--edition",
            "2024",
            "--test",
            "--crate-name",
            "nested_fixture_tests",
            "lib.rs",
            "-o",
            harness.to_str().unwrap(),
        ],
    );
    let tests = Command::new(&harness).output().unwrap();
    assert!(
        tests.status.success(),
        "the extracted tests module must still resolve `use super::helper`:\n{}",
        String::from_utf8_lossy(&tests.stdout)
    );
}

fn run_rustc(dir: &Path, args: &[&str]) {
    let output = Command::new("rustc")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("rustc must be runnable (it ships with cargo)");
    assert!(
        output.status.success(),
        "rustc {:?} failed:\n{}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}
