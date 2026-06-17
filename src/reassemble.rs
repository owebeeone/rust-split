//! Tier 2 step 2 — reassemble a planned split into output module files.
//!
//! Target: a **binary/crate-root** file (e.g. `src/main.rs`) whose items all
//! live in one module and reference each other by bare name. Splitting them
//! into sibling modules breaks those bare references, so the scheme is:
//!
//! - the crate root keeps the file preamble + imports, declares each part
//!   module, **`pub(crate) use part::*`** re-exports every part, and keeps
//!   `fn main` (the entry point must stay at the root);
//! - each part file over-includes the same import header, does `use crate::*`
//!   to see its siblings via the root re-exports, and carries its items with
//!   `pub(crate)` visibility bumped on so the re-exports can see them;
//! - an oversized `mod` (e.g. the test module) is extracted whole to its own
//!   file and declared at the root, keeping its `super::*` pointing at the root.
//!
//! Residual import/visibility that this mechanical scheme misses is left for the
//! compiler to enumerate (the documented O(n) finish) — but the LOC budget and
//! the byte-exact moves are done here.

use crate::{Exploded, SplitPlan, plan_split};
use syn::spanned::Spanned;

/// Over-include overhead added to each part beyond the shared header:
/// `use crate::*;` plus blank-line separators.
const PART_OVERHEAD: usize = 3;

/// Plan and reassemble a binary crate-root split so that **every output file is
/// `< max_loc`**. The item budget is reduced by the header (over-included into
/// each part) and the per-part overhead, so a packed part plus its header stays
/// under budget.
pub fn split_bin(exploded: &Exploded, max_loc: usize, root_stem: &str) -> SplitOutput {
    let header_loc: usize = exploded
        .manifest
        .rows
        .iter()
        .filter(|r| matches!(r.kind.as_str(), "use" | "preamble" | "extern_crate"))
        .map(|r| r.loc)
        .sum();
    let item_budget = max_loc.saturating_sub(header_loc + PART_OVERHEAD).max(1);
    let plan = plan_split(exploded, item_budget);
    reassemble_bin(exploded, &plan, root_stem, max_loc)
}

/// A file the split would write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    /// Path relative to the source file's directory (e.g. `main.rs`, `p00.rs`).
    pub path: String,
    pub contents: String,
    pub loc: usize,
}

/// The reassembled output plus the parts of the plan that could not be met.
#[derive(Debug, Clone)]
pub struct SplitOutput {
    pub files: Vec<OutputFile>,
    /// Names of items still at or above budget (e.g. an un-nested test mod).
    pub still_oversized: Vec<String>,
}

impl SplitOutput {
    /// The largest output file's LOC (the budget is met iff this is `< max_loc`
    /// and `still_oversized` is empty).
    pub fn max_loc(&self) -> usize {
        self.files.iter().map(|f| f.loc).max().unwrap_or(0)
    }
}

fn loc(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count().max(1)
    }
}

/// How the split file sits in the module tree — fixes the sibling path prefix
/// and the re-export visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topology {
    /// A binary crate root (`main.rs`): siblings reached via `crate::`,
    /// re-exported `pub(crate)`, `fn main` kept at the root.
    Bin,
    /// A nested library module (`foo/mod.rs`): sub-modules reached via `super::`,
    /// re-exported `pub` (to preserve the module's external API) *and*
    /// `pub(crate)` (for internal cross-references).
    Mod,
}

/// Plan and reassemble a nested library module (`foo/mod.rs`) so every output
/// file is `< max_loc`; sub-modules see each other via `super::*`.
pub fn split_mod(exploded: &Exploded, max_loc: usize, root_stem: &str) -> SplitOutput {
    let header_loc: usize = exploded
        .manifest
        .rows
        .iter()
        .filter(|r| matches!(r.kind.as_str(), "use" | "preamble" | "extern_crate"))
        .map(|r| r.loc)
        .sum();
    let item_budget = max_loc.saturating_sub(header_loc + PART_OVERHEAD).max(1);
    let plan = plan_split(exploded, item_budget);
    reassemble(exploded, &plan, root_stem, Topology::Mod, max_loc)
}

/// Reassemble a binary crate-root split (back-compat wrapper).
pub fn reassemble_bin(
    exploded: &Exploded,
    plan: &SplitPlan,
    root_stem: &str,
    max_loc: usize,
) -> SplitOutput {
    reassemble(exploded, plan, root_stem, Topology::Bin, max_loc)
}

/// Reassemble a split for the given module topology. `root_stem` is the source
/// file's stem (`"main"` for `main.rs`, `"mod"` for `foo/mod.rs`). `max_loc` is
/// the ceiling, needed to recursively split an oversized test module.
fn reassemble(
    exploded: &Exploded,
    plan: &SplitPlan,
    root_stem: &str,
    topology: Topology,
    max_loc: usize,
) -> SplitOutput {
    let sibling = match topology {
        Topology::Bin => "crate",
        Topology::Mod => "super",
    };
    let chunk_text = |i: usize| exploded.chunks[i].text.as_str();
    let row = |i: usize| {
        exploded
            .manifest
            .rows
            .iter()
            .find(|r| r.chunk_index == i)
            .unwrap()
    };

    // Shared header (preamble + imports), verbatim.
    let header: String = plan.header.iter().map(|&i| chunk_text(i)).collect();

    let mut files = Vec::new();
    let mut mod_decls = String::new();
    let mut reexports = String::new();
    let mut still_oversized = Vec::new();

    // Part modules, named after their cluster's dominant item.
    for part in &plan.parts {
        let module = if part.name == root_stem {
            format!("{}_", part.name)
        } else {
            part.name.clone()
        };
        let mut body = String::new();
        let mut root_main = None;
        for &i in &part.chunk_indices {
            let r = row(i);
            // `fn main` must stay at the crate root, not move into a submodule.
            if r.kind == "fn" && r.name == "main" {
                root_main = Some(i);
                continue;
            }
            body.push_str(&bump_visibility(chunk_text(i)));
        }
        // If the part was only `fn main`, it produced no module file.
        if body.trim().is_empty() {
            if let Some(i) = root_main {
                // stash main to emit at root via a sentinel part with no module
                files.push(OutputFile {
                    path: String::from("__root_main__"),
                    contents: chunk_text(i).to_owned(),
                    loc: 0,
                });
            }
            continue;
        }
        if let Some(i) = root_main {
            files.push(OutputFile {
                path: String::from("__root_main__"),
                contents: chunk_text(i).to_owned(),
                loc: 0,
            });
        }
        let contents = format!("{header}\nuse {sibling}::*;\n\n{body}");
        mod_decls.push_str(&format!("mod {module};\n"));
        match topology {
            Topology::Bin => reexports.push_str(&format!("pub(crate) use {module}::*;\n")),
            Topology::Mod => {
                // `pub` preserves the module's external API; `pub(crate)` lets
                // sibling sub-modules see internal (pub(crate)-bumped) items.
                reexports.push_str(&format!("pub use {module}::*;\n"));
                reexports.push_str(&format!("pub(crate) use {module}::*;\n"));
            }
        }
        let l = loc(&contents);
        files.push(OutputFile {
            path: format!("{module}.rs"),
            contents,
            loc: l,
        });
    }

    // Oversized items: a `mod` under the ceiling is extracted whole; a `mod`
    // over it is recursively split into a `{name}/` subdir; a leaf can't move.
    for over in &plan.oversized {
        if over.recoverable && over.kind == "mod" {
            let chunk = chunk_text(over.chunk_index);
            let (attrs, inner) = extract_mod(chunk, &over.name);
            let inner_loc = loc(&inner);
            mod_decls.push_str(&format!("{attrs}mod {};\n", over.name));
            if inner_loc < max_loc {
                files.push(OutputFile {
                    path: format!("{}.rs", over.name),
                    contents: inner,
                    loc: inner_loc,
                });
            } else {
                let (nested, nested_still) = split_nested_mod(&over.name, &inner, max_loc);
                files.extend(nested);
                still_oversized.extend(nested_still);
            }
        } else {
            still_oversized.push(format!(
                "{} ({} LOC {}, manual extraction)",
                over.name, over.loc, over.kind
            ));
        }
    }

    // Pull the stashed `fn main` (if any) and build the root file.
    let mut root_main = String::new();
    files.retain(|f| {
        if f.path == "__root_main__" {
            root_main = f.contents.clone();
            false
        } else {
            true
        }
    });

    let mut root = String::new();
    root.push_str(&header);
    root.push('\n');
    root.push_str(&mod_decls);
    root.push('\n');
    root.push_str(&reexports);
    if !root_main.is_empty() {
        root.push('\n');
        root.push_str(&root_main);
    }
    let root_loc = loc(&root);
    files.insert(
        0,
        OutputFile {
            path: format!("{root_stem}.rs"),
            contents: root,
            loc: root_loc,
        },
    );

    SplitOutput {
        files,
        still_oversized,
    }
}

/// Raise a moved item — and the struct fields / impl members referenced across
/// the new module boundaries — to `pub(crate)`, so the crate-root re-exports can
/// see them. Span-driven (parse the chunk, insert at exact offsets), so it is
/// attribute-safe and handles `fn` modifiers, multi-line attrs, etc. Items
/// already `pub`/`pub(crate)` are left alone. Over-exposes within the crate,
/// which is the mechanical, behavior-preserving choice.
fn bump_visibility(chunk_text: &str) -> String {
    let Ok(item) = syn::parse_str::<syn::Item>(chunk_text) else {
        return chunk_text.to_owned();
    };
    let mut offsets = Vec::new();
    collect_vis_offsets(&item, &mut offsets);
    apply_inserts(chunk_text, &offsets)
}

fn is_inherited(vis: &syn::Visibility) -> bool {
    matches!(vis, syn::Visibility::Inherited)
}

fn start<T: Spanned>(node: &T) -> usize {
    node.span().byte_range().start
}

/// Byte offset where a function's visibility belongs: before any
/// `const`/`async`/`unsafe`/`extern` modifier, else before `fn`.
fn fn_sig_start(sig: &syn::Signature) -> usize {
    let mut min = start(&sig.fn_token);
    if let Some(t) = &sig.constness {
        min = min.min(start(t));
    }
    if let Some(t) = &sig.asyncness {
        min = min.min(start(t));
    }
    if let Some(t) = &sig.unsafety {
        min = min.min(start(t));
    }
    if let Some(abi) = &sig.abi {
        min = min.min(start(&abi.extern_token));
    }
    min
}

fn field_offsets(fields: &syn::Fields, offsets: &mut Vec<usize>) {
    for field in fields {
        if is_inherited(&field.vis) {
            match &field.ident {
                Some(ident) => offsets.push(start(ident)),
                None => offsets.push(start(&field.ty)),
            }
        }
    }
}

fn collect_vis_offsets(item: &syn::Item, offsets: &mut Vec<usize>) {
    use syn::Item;
    match item {
        Item::Fn(f) => {
            if is_inherited(&f.vis) {
                offsets.push(fn_sig_start(&f.sig));
            }
        }
        Item::Struct(s) => {
            if is_inherited(&s.vis) {
                offsets.push(start(&s.struct_token));
            }
            field_offsets(&s.fields, offsets);
        }
        Item::Enum(e) => {
            if is_inherited(&e.vis) {
                offsets.push(start(&e.enum_token));
            }
        }
        Item::Union(u) => {
            if is_inherited(&u.vis) {
                offsets.push(start(&u.union_token));
            }
            field_offsets(&syn::Fields::Named(u.fields.clone()), offsets);
        }
        Item::Const(c) if is_inherited(&c.vis) => offsets.push(start(&c.const_token)),
        Item::Static(s) if is_inherited(&s.vis) => offsets.push(start(&s.static_token)),
        Item::Type(t) if is_inherited(&t.vis) => offsets.push(start(&t.type_token)),
        Item::Trait(t) if is_inherited(&t.vis) => offsets.push(start(&t.trait_token)),
        Item::TraitAlias(t) if is_inherited(&t.vis) => offsets.push(start(&t.trait_token)),
        // Only inherent impls (`impl Type`) allow member visibility; trait-impl
        // members (`impl Trait for Type`) inherit the trait's and must not.
        Item::Impl(i) if i.trait_.is_none() => {
            for member in &i.items {
                match member {
                    syn::ImplItem::Fn(f) if is_inherited(&f.vis) => {
                        offsets.push(fn_sig_start(&f.sig))
                    }
                    syn::ImplItem::Const(c) if is_inherited(&c.vis) => {
                        offsets.push(start(&c.const_token))
                    }
                    syn::ImplItem::Type(t) if is_inherited(&t.vis) => {
                        offsets.push(start(&t.type_token))
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn apply_inserts(text: &str, offsets: &[usize]) -> String {
    let mut offsets: Vec<usize> = offsets.to_vec();
    offsets.sort_unstable();
    offsets.dedup();
    let mut out = String::with_capacity(text.len() + offsets.len() * 11);
    let mut prev = 0;
    for &offset in &offsets {
        out.push_str(&text[prev..offset]);
        out.push_str("pub(crate) ");
        prev = offset;
    }
    out.push_str(&text[prev..]);
    out
}

/// Split a `mod NAME { ... }` chunk into its leading attrs (for the root decl)
/// and its inner body (for the extracted file). Best-effort, line-based.
fn extract_mod(chunk_text: &str, name: &str) -> (String, String) {
    let open = format!("mod {name}");
    let lines: Vec<&str> = chunk_text.split_inclusive('\n').collect();
    let mut decl_line = None;
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with(&open) {
            decl_line = Some(i);
            break;
        }
    }
    let Some(decl) = decl_line else {
        return (String::new(), chunk_text.to_owned());
    };
    // attrs = the `#[...]` lines immediately above the decl (skip blank/comment gap)
    let mut attrs = String::new();
    for line in &lines[..decl] {
        let t = line.trim_start();
        if t.starts_with("#[") || t.starts_with("///") || t.starts_with("//!") {
            attrs.push_str(line);
        }
    }
    // inner = everything after the decl line up to (not including) the final `}`
    let mut inner_lines = &lines[decl + 1..];
    while inner_lines
        .last()
        .map(|l| l.trim().is_empty())
        .unwrap_or(false)
    {
        inner_lines = &inner_lines[..inner_lines.len() - 1];
    }
    // drop the closing `}` of the mod (last non-blank line)
    if inner_lines.last().map(|l| l.trim() == "}").unwrap_or(false) {
        inner_lines = &inner_lines[..inner_lines.len() - 1];
    }
    (attrs, inner_lines.concat())
}

/// Recursively split an over-budget module's inner body into a `{name}/`
/// subdirectory: `{name}/mod.rs` forwards the enclosing module's items
/// (`pub(crate) use super::*`) and re-exports the clusters; each `{name}/gNN.rs`
/// holds a cohesive cluster and reaches everything via `super::*`. Used for big
/// test modules.
fn split_nested_mod(name: &str, inner: &str, max_loc: usize) -> (Vec<OutputFile>, Vec<String>) {
    let Ok(exploded) = crate::explode(inner) else {
        return (
            vec![OutputFile {
                path: format!("{name}.rs"),
                contents: inner.to_owned(),
                loc: loc(inner),
            }],
            vec![format!("{name} (could not parse for nested split)")],
        );
    };

    // Inner header = the module's own imports, minus `use super::*` (the parent
    // glob — `{name}/mod.rs` forwards that instead).
    let is_super_glob = |t: &str| t.replace(' ', "").contains("usesuper::*;");
    let group_header: String = exploded
        .manifest
        .rows
        .iter()
        .filter(|r| matches!(r.kind.as_str(), "use" | "preamble" | "extern_crate"))
        .map(|r| exploded.chunks[r.chunk_index].text.as_str())
        .filter(|t| !is_super_glob(t))
        .collect();
    let header_loc = loc(&group_header);
    let item_budget = max_loc.saturating_sub(header_loc + PART_OVERHEAD).max(1);
    let plan = plan_split(&exploded, item_budget);

    let mut files = Vec::new();
    let mut decls = String::new();
    let mut reexports = String::new();
    let mut still = Vec::new();
    for (idx, part) in plan.parts.iter().enumerate() {
        let module = format!("g{idx:02}");
        let mut body = String::new();
        for &ci in &part.chunk_indices {
            body.push_str(&bump_visibility(exploded.chunks[ci].text.as_str()));
        }
        let contents = format!("{group_header}\nuse super::*;\n\n{body}");
        files.push(OutputFile {
            path: format!("{name}/{module}.rs"),
            loc: loc(&contents),
            contents,
        });
        decls.push_str(&format!("mod {module};\n"));
        reexports.push_str(&format!("pub(crate) use {module}::*;\n"));
    }
    for over in &plan.oversized {
        still.push(format!(
            "{name}::{} ({} LOC {}, leaf needs manual extraction)",
            over.name, over.loc, over.kind
        ));
    }

    let modrs = format!("pub(crate) use super::*;\n\n{decls}\n{reexports}");
    files.insert(
        0,
        OutputFile {
            path: format!("{name}/mod.rs"),
            loc: loc(&modrs),
            contents: modrs,
        },
    );
    (files, still)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{explode, plan_split};

    #[test]
    fn splits_a_flat_bin_root_into_cohesive_under_budget_modules() {
        // four cohesive groups (g{k}_0 -> g{k}_1 -> g{k}_2 -> g{k}_3); main calls g0_0
        let mut src = String::from("use std::fmt;\n\n");
        for k in 0..4 {
            for n in 0..4 {
                let call = if n < 3 {
                    format!("    g{k}_{}();\n", n + 1)
                } else {
                    String::new()
                };
                src.push_str(&format!("fn g{k}_{n}() {{\n{call}}}\n\n"));
            }
        }
        src.push_str("fn main() {\n    g0_0();\n}\n");
        let exploded = explode(&src).unwrap();
        let max_loc = 60;
        let out = split_bin(&exploded, max_loc, "main");

        assert!(out.still_oversized.is_empty());
        for f in &out.files {
            assert!(f.loc < max_loc, "{} over budget: {}", f.path, f.loc);
        }
        let root = out.files.iter().find(|f| f.path == "main.rs").unwrap();
        assert!(root.contents.contains("fn main"), "main stays at root");
        assert!(root.contents.contains("pub(crate) use "));
        let part = out.files.iter().find(|f| f.path != "main.rs").unwrap();
        assert!(part.contents.contains("use crate::*;"));
        assert!(part.contents.contains("pub(crate) fn "));
        // cohesion: the whole g0 group lands in ONE module
        let g0 = out
            .files
            .iter()
            .find(|f| f.contents.contains("fn g0_1"))
            .unwrap();
        for n in 0..4 {
            assert!(
                g0.contents.contains(&format!("fn g0_{n}")),
                "g0 group together"
            );
        }
    }

    #[test]
    fn oversized_mod_is_extracted_to_its_own_file() {
        let mut inner = String::new();
        for i in 0..10 {
            inner.push_str(&format!("    fn t{i}() {{}}\n"));
        }
        let src = format!("fn main() {{}}\n\n#[cfg(test)]\nmod tests {{\n{inner}}}\n");
        let exploded = explode(&src).unwrap();
        let plan = plan_split(&exploded, 6);
        // ceiling large enough that the test mod fits whole -> single tests.rs
        let out = reassemble_bin(&exploded, &plan, "main", 1000);
        let tests = out.files.iter().find(|f| f.path == "tests.rs").unwrap();
        assert!(tests.contents.contains("fn t0()"));
        assert!(!tests.contents.contains("mod tests"));
        let root = out.files.iter().find(|f| f.path == "main.rs").unwrap();
        assert!(root.contents.contains("#[cfg(test)]\nmod tests;"));
    }

    #[test]
    fn an_over_ceiling_test_mod_is_nested_into_a_subdir() {
        // 30 test fns referencing a shared helper -> test mod far over a tiny ceiling
        let mut inner =
            String::from("    use super::*;\n\n    fn helper() -> i32 {\n        1\n    }\n");
        for i in 0..30 {
            inner.push_str(&format!(
                "\n    #[test]\n    fn t{i}() {{\n        assert_eq!(helper(), 1);\n    }}\n"
            ));
        }
        let src = format!(
            "pub fn helper() -> i32 {{\n    1\n}}\n\nfn main() {{}}\n\n#[cfg(test)]\nmod tests {{\n{inner}}}\n"
        );
        let exploded = explode(&src).unwrap();
        let out = split_bin(&exploded, 60, "main");

        // the test mod became a `tests/` subdir, every file under the ceiling
        let mod_rs = out.files.iter().find(|f| f.path == "tests/mod.rs").unwrap();
        assert!(
            mod_rs.contents.contains("pub(crate) use super::*;"),
            "forwards the parent"
        );
        assert!(mod_rs.contents.contains("mod g00;"));
        assert!(out.files.iter().any(|f| f.path == "tests/g00.rs"));
        for f in &out.files {
            assert!(f.loc < 60, "{} over ceiling: {}", f.path, f.loc);
        }
        let root = out.files.iter().find(|f| f.path == "main.rs").unwrap();
        assert!(root.contents.contains("#[cfg(test)]\nmod tests;"));
    }

    #[test]
    fn split_mod_uses_super_and_preserves_pub_api() {
        // alpha (pub, the module's API) calls beta (private helper) -> one cluster
        let src = "use std::fmt;\n\npub fn alpha() -> i32 {\n    beta()\n}\n\nfn beta() -> i32 {\n    1\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_mod(&exploded, 10_000, "mod");

        let root = out.files.iter().find(|f| f.path == "mod.rs").unwrap();
        assert!(
            root.contents.contains("pub use "),
            "pub re-export preserves the API"
        );
        assert!(
            root.contents.contains("pub(crate) use "),
            "pub(crate) for internal refs"
        );

        let sub = out.files.iter().find(|f| f.path != "mod.rs").unwrap();
        assert!(
            sub.contents.contains("use super::*;"),
            "siblings reached via super, not crate"
        );
        assert!(!sub.contents.contains("use crate::*;"));
        assert!(sub.contents.contains("pub fn alpha"), "pub item stays pub");
        assert!(
            sub.contents.contains("pub(crate) fn beta"),
            "private bumped to pub(crate)"
        );
    }
}
