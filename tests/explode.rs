use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rust_split::{Exploded, ManifestRow, explode};

fn assert_tiling(src: &str, exploded: &Exploded) {
    let mut cursor = 0;
    let mut joined = String::new();
    for chunk in &exploded.chunks {
        assert_eq!(chunk.byte_range.start, cursor);
        assert!(chunk.byte_range.end >= chunk.byte_range.start);
        assert_eq!(
            chunk.text,
            &src[chunk.byte_range.start..chunk.byte_range.end]
        );
        cursor = chunk.byte_range.end;
        joined.push_str(&chunk.text);
    }
    assert_eq!(cursor, src.len());
    assert_eq!(joined, src);
}

fn row<'a>(exploded: &'a Exploded, name: &str) -> &'a ManifestRow {
    exploded
        .manifest
        .rows
        .iter()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("missing manifest row {name:?}"))
}

fn chunk_text<'a>(exploded: &'a Exploded, row: &ManifestRow) -> &'a str {
    &exploded.chunks[row.chunk_index].text
}

fn span_text<'a>(src: &'a str, row: &ManifestRow) -> &'a str {
    &src[row.span.start..row.span.end]
}

fn fixture_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    collect_rs(root, &mut paths);
    paths.sort();
    paths
}

fn expected_real_fixture_paths() -> Vec<&'static str> {
    vec![
        "gwz-cli/src/main.rs",
        "gwz-cli/tests/local_workflows.rs",
        "gwz-cli/tests/publish_workflow.rs",
        "gwz-cli/tests/rename.rs",
        "gwz-core/protocol/corpus/rust/vectors.rs",
        "gwz-core/src/artifact/mod.rs",
        "gwz-core/src/cbor.rs",
        "gwz-core/src/git/mod.rs",
        "gwz-core/src/lib.rs",
        "gwz-core/src/model/mod.rs",
        "gwz-core/src/operation/mod.rs",
        "gwz-core/src/protocol/convert.rs",
        "gwz-core/src/protocol/generated.rs",
        "gwz-core/src/protocol/mod.rs",
        "gwz-core/src/runtime/clock.rs",
        "gwz-core/src/runtime/ids.rs",
        "gwz-core/src/runtime/mod.rs",
        "gwz-core/src/status/mod.rs",
        "gwz-core/src/workspace/mod.rs",
        "gwz-core/src/workspace_ops/mod.rs",
        "gwz-core/tests/protocol.rs",
        "gwz-core/tests/publish_workflow.rs",
        "gwz-core/tests/rename.rs",
    ]
}

fn collect_rs(path: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn whole_gwz_fixture_corpus_tiles_losslessly() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real");
    let paths = fixture_paths(&root);
    let actual = paths
        .iter()
        .map(|path| {
            path.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected_real_fixture_paths());

    for path in paths {
        let src = fs::read_to_string(&path).unwrap();
        let exploded = explode(&src)
            .unwrap_or_else(|error| panic!("failed to explode {}: {error}", path.display()));
        assert_tiling(&src, &exploded);
        assert_eq!(
            explode(&src).unwrap(),
            exploded,
            "not deterministic for {}",
            path.display()
        );
    }
}

#[test]
fn doc_comments_and_outer_attrs_belong_to_the_item() {
    let src = "/// docs\n#[allow(dead_code)]\nfn documented() {}\n";
    let exploded = explode(src).unwrap();
    assert_tiling(src, &exploded);

    let documented = row(&exploded, "documented");
    assert_eq!(documented.kind, "fn");
    assert_eq!(chunk_text(&exploded, documented), src);
    assert!(span_text(src, documented).starts_with("/// docs"));
}

#[test]
fn inner_attrs_are_preamble_not_item_owned() {
    let src = "//! module docs\n#![allow(dead_code)]\nfn item() {}\n";
    let exploded = explode(src).unwrap();
    assert_tiling(src, &exploded);

    assert_eq!(exploded.chunks.len(), 2);
    assert_eq!(exploded.manifest.rows[0].kind, "preamble");
    assert!(exploded.chunks[0].text.contains("//! module docs"));

    let item = row(&exploded, "item");
    assert_eq!(chunk_text(&exploded, item), "fn item() {}\n");
    assert_eq!(span_text(src, item), "fn item() {}");
}

#[test]
fn free_comment_gap_attaches_to_following_item_after_last_blank_line() {
    let src = "fn a() {}\n\n// belongs to b\nfn b() {}\n";
    let exploded = explode(src).unwrap();
    assert_tiling(src, &exploded);

    let a = row(&exploded, "a");
    let b = row(&exploded, "b");
    assert_eq!(chunk_text(&exploded, a), "fn a() {}\n\n");
    assert_eq!(chunk_text(&exploded, b), "// belongs to b\nfn b() {}\n");
}

#[test]
fn leading_license_without_inner_attrs_attaches_to_first_item() {
    let src = "// Copyright\n// SPDX-License-Identifier: GPL-2.0-only\nfn first() {}\n";
    let exploded = explode(src).unwrap();
    assert_tiling(src, &exploded);

    assert_eq!(exploded.chunks.len(), 1);
    assert_eq!(chunk_text(&exploded, row(&exploded, "first")), src);
}

#[test]
fn trailing_comment_and_final_newline_attach_to_last_item() {
    let src = "fn last() {}\n// trailing\n";
    let exploded = explode(src).unwrap();
    assert_tiling(src, &exploded);

    assert_eq!(chunk_text(&exploded, row(&exploded, "last")), src);
}

#[test]
fn macro_impl_test_mod_crlf_and_unicode_boundaries_are_stable() {
    let src = "#[cfg(test)]\r\nmod tests {\r\n    #[test]\r\n    fn it_works() {}\r\n}\r\n\r\nmacro_rules! say_hi {\r\n    () => { println!(\"hi\"); };\r\n}\r\n\r\ntrait Hello {}\r\nstruct Café;\r\nimpl Hello for Café {}\r\n";
    let exploded = explode(src).unwrap();
    assert_tiling(src, &exploded);

    assert_eq!(row(&exploded, "tests").kind, "mod");
    assert_eq!(row(&exploded, "say_hi").kind, "macro_rules");
    assert_eq!(row(&exploded, "impl Hello for Café").kind, "impl");
    assert_eq!(
        span_text(src, row(&exploded, "impl Hello for Café")),
        "impl Hello for Café {}"
    );
}

#[test]
fn empty_comment_only_inner_attr_only_and_use_only_files_are_handled() {
    for src in ["", "// only\n", "//! only\n#![allow(dead_code)]\n"] {
        let exploded = explode(src).unwrap();
        assert_tiling(src, &exploded);
        if src.is_empty() {
            assert!(exploded.chunks.is_empty());
        } else {
            assert_eq!(exploded.chunks.len(), 1);
            assert_eq!(exploded.manifest.rows[0].kind, "preamble");
        }
    }

    let use_only = "use std::{fmt, io};\n";
    let exploded = explode(use_only).unwrap();
    assert_tiling(use_only, &exploded);
    assert_eq!(row(&exploded, "std::{ fmt, io }").kind, "use");
}

#[test]
fn manifest_names_and_kinds_cover_top_level_item_variants() {
    let src = r#"
extern crate core;
use std::fmt;
const C: usize = 1;
static S: usize = 2;
type Alias = usize;
struct Struct;
union MyUnion { a: usize }
enum Enum { A }
trait Trait { fn method(&self); }
trait AliasTrait = Trait;
fn function() {}
mod module {}
extern "C" { fn foreign(); }
impl Trait for Struct { fn method(&self) {} }
macro_rules! rules { () => {}; }
custom_macro! { tokens }
"#;

    let exploded = explode(src).unwrap();
    assert_tiling(src, &exploded);

    let expected = [
        ("core", "extern_crate"),
        ("std::fmt", "use"),
        ("C", "const"),
        ("S", "static"),
        ("Alias", "type"),
        ("Struct", "struct"),
        ("MyUnion", "union"),
        ("Enum", "enum"),
        ("Trait", "trait"),
        ("AliasTrait", "trait_alias"),
        ("function", "fn"),
        ("module", "mod"),
        ("extern \"C\"", "foreign_mod"),
        ("impl Trait for Struct", "impl"),
        ("rules", "macro_rules"),
        ("custom_macro", "macro"),
    ];

    for (name, kind) in expected {
        let row = row(&exploded, name);
        assert_eq!(row.kind, kind, "row {name}");
        assert!(row.loc >= 1);
        assert!(row.span.end > row.span.start);
    }
}

#[test]
fn adjacency_hint_is_syntactic_and_can_include_shadowed_sibling_names() {
    let src = "fn target() {}\nfn caller() { target(); }\nfn shadow() { let target = 1; let _ = target; }\nfn unrelated() {}\n";
    let exploded = explode(src).unwrap();
    assert_tiling(src, &exploded);

    assert_eq!(row(&exploded, "caller").adjacency_hint, vec!["target"]);
    assert_eq!(row(&exploded, "shadow").adjacency_hint, vec!["target"]);
    assert!(row(&exploded, "unrelated").adjacency_hint.is_empty());
}

#[test]
fn unparseable_input_returns_error_without_panic() {
    let error = explode("fn broken(").unwrap_err();
    assert!(error.to_string().contains("parse"));
}

#[test]
fn cli_explode_writes_chunks_and_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("input.rs");
    let out = temp.path().join("out");
    fs::write(&input, "fn a() {}\n\nfn b() {}\n").unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_rust-split"))
        .arg("explode")
        .arg(&input)
        .arg("--out")
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());

    assert_eq!(
        fs::read_to_string(out.join("chunk-000.rs")).unwrap(),
        "fn a() {}\n\n"
    );
    assert_eq!(
        fs::read_to_string(out.join("chunk-001.rs")).unwrap(),
        "fn b() {}\n"
    );

    let manifest = fs::read_to_string(out.join("manifest.toml")).unwrap();
    assert!(manifest.contains("name = \"a\""));
    assert!(manifest.contains("name = \"b\""));
    assert!(manifest.contains("kind = \"fn\""));
}
