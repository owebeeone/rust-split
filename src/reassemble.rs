//! Tier 2 step 2 — reassemble a planned split into output module files.
//!
//! Target: a **binary/crate-root** file (e.g. `src/main.rs`) whose items all
//! live in one module and reference each other by bare name. Splitting them
//! into sibling modules breaks those bare references, so the scheme is:
//!
//! - the crate root keeps the file preamble + imports, the source's own
//!   re-exports (`pub use ...` — dropping or demoting one would narrow the
//!   crate's public API) and content-less `mod name;` declarations verbatim,
//!   declares each part module, re-exports each part with a single glob whose
//!   visibility matches the part's widest item (`pub use` when the part has a
//!   public item, else `pub(crate) use`, and nothing for an `impl`-only part),
//!   and keeps `fn main` (the entry point must stay at the root);
//! - each part file copies the imports it references by name (preamble stays at
//!   the root), does `use crate::*` to see its siblings via the root re-exports,
//!   and carries its items with `pub(crate)` visibility bumped on so the
//!   re-exports can see them;
//! - a body-carrying `mod` (e.g. the test module) of **any size** is extracted
//!   whole to its own file — its attributes (`#[cfg(test)]` must keep gating
//!   it) and visibility travel to the root declaration, and its `super::*`
//!   keeps pointing at the root; one over the ceiling is split further into a
//!   `{name}/` subdirectory.
//!
//! Residual import/visibility that this mechanical scheme misses is left for the
//! compiler to enumerate (the documented O(n) finish) — but the LOC budget and
//! the byte-exact moves are done here.

use crate::{Exploded, SplitPlan, plan_split};
use syn::parse::Parser;
use syn::spanned::Spanned;

/// Per-part overhead beyond the imports: the `use crate::*;` sibling glob plus
/// blank-line separators.
const PART_OVERHEAD: usize = 3;

/// Plan and reassemble a binary crate-root split so that **every output file is
/// `< max_loc`**. The item budget reserves the full header LOC and the per-part
/// overhead; since a part now copies only the imports it references (a subset of
/// the header), this reservation is conservative — a packed part stays safely
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
/// and where sub-module files go. Re-export visibility is per part in both
/// topologies: `pub use` when the part has a public item (preserving the
/// crate's public API), else `pub(crate) use`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topology {
    /// A binary or library crate root (`main.rs`, `lib.rs`): siblings reached
    /// via `crate::`, `fn main` (if any) kept at the root.
    Bin,
    /// A nested library module (`foo/mod.rs`, or a plain `foo.rs` file module):
    /// sub-modules reached via `super::` and re-exported with a single per-module
    /// glob — `pub use` to preserve a public API, else `pub(crate) use`. A plain
    /// `foo.rs` file module resolves `mod bar;` to `foo/bar.rs`, so its
    /// sub-modules are written into a `foo/` subdir.
    Mod,
}

/// Plan and reassemble a library module so every output file is `< max_loc`;
/// sub-modules see each other via `super::*`. Handles both a directory-owning
/// `foo/mod.rs` (sub-modules become siblings) and a plain `foo.rs` file module
/// (sub-modules go in a `foo/` subdir). `root_stem` is the source file's stem.
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
    // A part is a module *below* the file it came from, whichever topology, so
    // the header imports it copies have to be re-anchored one level up.
    let part_rebase = match topology {
        Topology::Bin => Rebase::CrateRoot,
        Topology::Mod => Rebase::Nested,
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

    // Where sub-module files go. A directory-owning module file
    // (`main.rs`/`lib.rs`/`foo/mod.rs`) keeps them as siblings; a plain `foo.rs`
    // file module resolves `mod bar;` to `foo/bar.rs`, so its parts go in `foo/`.
    let sub_prefix = match topology {
        Topology::Bin => String::new(),
        Topology::Mod if matches!(root_stem, "mod" | "lib" | "main") => String::new(),
        Topology::Mod => format!("{root_stem}/"),
    };

    // The crate/module preamble (inner `//!` docs / `#![...]` attrs) stays at
    // the root only — a crate-root `#![feature]` is a hard error in a sub-module
    // file, and a crate-root `#![allow]` already covers descendants. The import
    // chunks (`use`/`extern crate`) are copied into each file, but only the ones
    // that file references by name (see `select_imports`). Re-exports
    // (`pub use ...`) are not imports — the planner routes them to
    // `plan.root_items`, kept at the root verbatim.
    let preamble: String = plan
        .header
        .iter()
        .filter(|&&i| row(i).kind == "preamble")
        .map(|&i| chunk_text(i))
        .collect();
    let import_chunks: Vec<usize> = plan
        .header
        .iter()
        .copied()
        .filter(|&i| row(i).kind != "preamble")
        .collect();

    // Source chunks that stay at the root verbatim: `pub use` re-exports (the
    // crate's public surface) and content-less `mod name;` declarations (which
    // bind files relative to the root's directory).
    let root_kept: String = plan.root_items.iter().map(|&i| chunk_text(i)).collect();

    // Module names fixed by the source — extracted body-mods and `mod name;`
    // declarations keep their own names — plus the root's stem; a generated
    // part that would collide is renamed with a `_` suffix.
    let mut reserved: std::collections::BTreeSet<&str> =
        plan.mods.iter().map(|&i| row(i).name.as_str()).collect();
    reserved.insert(root_stem);
    for &i in &plan.root_items {
        if row(i).kind == "mod" {
            reserved.insert(row(i).name.as_str());
        }
    }

    let mut files = Vec::new();
    let mut mod_decls = String::new();
    let mut reexports = String::new();
    let mut still_oversized = Vec::new();
    // Names referenced from the root file itself (`fn main`, extracted module
    // bodies whose `use super::*` reaches the root's imports).
    let mut root_refs = Refs::default();

    // Part modules, named after their cluster's dominant item.
    for part in &plan.parts {
        let module = if reserved.contains(part.name.as_str()) {
            format!("{}_", part.name)
        } else {
            part.name.clone()
        };
        let mut body = String::new();
        let mut root_main = None;
        // Widest visibility any moved item contributes to a glob re-export.
        let mut export_vis = ExportVis::None;
        // Names this part references, so only the imports it uses are copied in.
        let mut refs = Refs::default();
        for &i in &part.chunk_indices {
            let r = row(i);
            // `fn main` must stay at the crate root, not move into a submodule.
            if r.kind == "fn" && r.name == "main" {
                root_main = Some(i);
                continue;
            }
            export_vis = export_vis.max(item_export_vis(chunk_text(i)));
            refs.add(chunk_text(i));
            body.push_str(&rebase_body_paths(
                &bump_visibility(chunk_text(i)),
                part_rebase,
            ));
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
        let part_imports = select_imports(&import_chunks, &exploded.chunks, &refs, part_rebase);
        let contents = part_file(&part_imports, sibling, &body);
        mod_decls.push_str(&format!("mod {module};\n"));
        // One glob re-export per module, visibility chosen by the module's widest
        // item: `pub use` when it has a public item (a `pub use *` caps each item
        // at its own visibility, so it covers internal `pub(crate)` items in the
        // same line); else `pub(crate) use`; and nothing when the module exposes
        // no nameable item (e.g. a bare `impl` — a glob re-export would only warn
        // "doesn't reexport anything"). This keeps the crate's public surface
        // (`pub use`) reachable and avoids the redundant double re-export.
        if let Some(line) = reexport_line(export_vis, &module) {
            reexports.push_str(&line);
        }
        let l = loc(&contents);
        files.push(OutputFile {
            path: format!("{sub_prefix}{module}.rs"),
            contents,
            loc: l,
        });
    }

    // Body-carrying mods are extracted whole, whatever their size: packing a
    // `#[cfg(test)] mod tests { ... }` into a cluster would nest it inside a
    // part module and leave the root declaration ungated. The declaration —
    // leading comments, attributes, visibility — travels to the root; the
    // unwrapped body becomes the module's file, split further into a `{name}/`
    // subdir when it is itself over the ceiling.
    for &mod_index in &plan.mods {
        let chunk = chunk_text(mod_index);
        let name = row(mod_index).name.as_str();
        let Some((decl, inner)) = extract_mod(chunk) else {
            // Should not happen (the planner classified it by parsing); keep
            // the chunk intact at the root rather than lose it.
            mod_decls.push_str(chunk);
            continue;
        };
        mod_decls.push_str(&decl);
        // The body's `use super::*;` reaches the root's imports, so the root
        // must keep the imports the body names.
        root_refs.add_source(&inner);
        let inner_loc = loc(&inner);
        if inner_loc < max_loc {
            files.push(OutputFile {
                path: format!("{sub_prefix}{name}.rs"),
                contents: inner,
                loc: inner_loc,
            });
        } else {
            let (nested, nested_still) = split_nested_mod(name, &inner, max_loc);
            files.extend(nested.into_iter().map(|mut f| {
                f.path = format!("{sub_prefix}{}", f.path);
                f
            }));
            still_oversized.extend(nested_still);
        }
    }

    // Oversized leaves can't be moved mechanically.
    for over in &plan.oversized {
        still_oversized.push(format!(
            "{} ({} LOC {}, manual extraction)",
            over.name, over.loc, over.kind
        ));
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

    // The root keeps the preamble, then only the imports its own retained code
    // references — `fn main` (if any) plus extracted module bodies, whose
    // `use super::*;` resolves against the root. A root that is just module
    // declarations and re-exports needs no imports at all.
    if !root_main.trim().is_empty() {
        root_refs.add(&root_main);
    }
    let root_imports = if root_refs.is_empty() {
        String::new()
    } else {
        // The root file *is* the original module: its own imports still mean
        // what they meant.
        select_imports(&import_chunks, &exploded.chunks, &root_refs, Rebase::Keep)
    };

    let mut root = String::new();
    root.push_str(&preamble);
    root.push_str(&root_imports);
    root.push_str(&root_kept);
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

    // `include!`/`include_str!`/`include_bytes!` resolve their argument against
    // the directory of the file that contains them, so a body written into a
    // subdirectory needs one `../` per level it moved down. The root file did
    // not move (no `/` in its path), and neither did a part that stayed a
    // sibling. Inserting `../` adds no lines, so `loc` is unaffected.
    for file in &mut files {
        let levels = file.path.matches('/').count();
        if levels == 0 || !file.contents.contains("include") {
            continue;
        }
        file.contents = rebase_includes(&file.contents, levels, &file.path);
    }

    SplitOutput {
        files,
        still_oversized,
    }
}

/// Macros whose single argument is a path resolved relative to the *file* that
/// contains them — the one construct a byte-exact move cannot leave alone.
const PATH_MACROS: [&str; 3] = ["include", "include_str", "include_bytes"];

/// Re-base the relative path arguments of the `include*!` macros in a file
/// written `levels` directories below the source file. Token-driven, so a path
/// spelled inside a comment or an unrelated string is never touched. An
/// absolute path is left alone; so is a non-literal argument (`concat!`,
/// `env!`, a macro, a constant), which gets one warning line on stderr naming
/// the generated file and line so the user knows where to look.
fn rebase_includes(contents: &str, levels: usize, path: &str) -> String {
    let Ok(stream) = contents.parse::<proc_macro2::TokenStream>() else {
        return contents.to_owned();
    };
    let mut edits = Vec::new();
    let prefix = "../".repeat(levels);
    collect_include_edits(stream, &prefix, contents, path, &mut edits);
    apply_replacements(contents, &edits)
}

/// Walk a token stream for `include*!(...)` calls, recording where a `../`
/// prefix must be inserted and warning about the arguments that cannot be
/// re-based mechanically.
fn collect_include_edits<'a>(
    stream: proc_macro2::TokenStream,
    prefix: &'a str,
    contents: &str,
    path: &str,
    edits: &mut Vec<(std::ops::Range<usize>, &'a str)>,
) {
    let trees: Vec<proc_macro2::TokenTree> = stream.into_iter().collect();
    for (index, tree) in trees.iter().enumerate() {
        let proc_macro2::TokenTree::Group(group) = tree else {
            continue;
        };
        if !is_path_macro_call(&trees, index) {
            collect_include_edits(group.stream(), prefix, contents, path, edits);
            continue;
        }
        match literal_insert_point(&group.stream()) {
            Some(Some(offset)) => edits.push((offset..offset, prefix)),
            // A literal that must not move (absolute path): nothing to do.
            Some(None) => {}
            None => {
                let line = contents[..group.span().byte_range().start].lines().count();
                eprintln!(
                    "rust-split: {path}:{line}: {}! argument is not a plain string \
                     literal — its path was not re-based",
                    trees[index - 2]
                );
            }
        }
    }
}

/// Whether the group at `index` is the argument list of an `include*!` call,
/// i.e. the two preceding tokens are the macro name and its `!`.
fn is_path_macro_call(trees: &[proc_macro2::TokenTree], index: usize) -> bool {
    if index < 2 {
        return false;
    }
    let proc_macro2::TokenTree::Ident(name) = &trees[index - 2] else {
        return false;
    };
    let proc_macro2::TokenTree::Punct(bang) = &trees[index - 1] else {
        return false;
    };
    bang.as_char() == '!' && PATH_MACROS.iter().any(|m| name == m)
}

/// For an `include*!` argument list: `Some(Some(offset))` is the byte offset
/// just inside the opening quote of a re-basable relative path literal,
/// `Some(None)` a literal that must stay put (an absolute path), and `None` an
/// argument that is not a single plain string literal with an optional comma.
fn literal_insert_point(stream: &proc_macro2::TokenStream) -> Option<Option<usize>> {
    let mut trees = stream.clone().into_iter();
    let proc_macro2::TokenTree::Literal(literal) = trees.next()? else {
        return None;
    };
    if let Some(token) = trees.next()
        && (!matches!(token, proc_macro2::TokenTree::Punct(p) if p.as_char() == ',')
            || trees.next().is_some())
    {
        return None;
    }
    let text = literal.to_string();
    let value = syn::parse_str::<syn::LitStr>(&text).ok()?.value();
    // An absolute path does not move with the file; an empty one is not a path.
    let windows_drive =
        matches!(value.as_bytes(), [drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic());
    if value.is_empty() || value.starts_with('/') || value.starts_with(r"\\") || windows_drive {
        return Some(None);
    }
    // Insert just inside the opening quote — `"…"`, `r"…"` and `r#"…"#` alike —
    // so the literal's own escaping is preserved byte-for-byte.
    let quote = text.find('"')?;
    Some(Some(literal.span().byte_range().start + quote + 1))
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

/// Assemble a part file: its selected imports, the sibling glob so it can reach
/// the rest of the module via the root re-exports, then the moved items.
fn part_file(imports: &str, sibling: &str, body: &str) -> String {
    if imports.trim().is_empty() {
        format!("use {sibling}::*;\n\n{body}")
    } else {
        format!("{imports}\nuse {sibling}::*;\n\n{body}")
    }
}

/// The glob re-export line for a module given the widest visibility of its
/// items — or `None` when it exposes nothing nameable (re-exporting an
/// `impl`-only module would only warn).
fn reexport_line(vis: ExportVis, module: &str) -> Option<String> {
    match vis {
        ExportVis::None => None,
        ExportVis::Crate => Some(format!("pub(crate) use {module}::*;\n")),
        ExportVis::Pub => Some(format!("pub use {module}::*;\n")),
    }
}

/// What a moved item contributes to its module's glob re-export. Ordered so the
/// widest contribution across a module's items picks the re-export visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ExportVis {
    /// No nameable export (a bare `impl`, a macro invocation, parse failure).
    None,
    /// A crate-visible name: a private item (bumped to `pub(crate)`), or one
    /// already `pub(crate)`/`pub(super)`/`pub(in …)`.
    Crate,
    /// A fully public name — part of the crate/module's public surface.
    Pub,
}

/// Classify one chunk's contribution to its module's glob re-export, mirroring
/// `bump_visibility`: items it raises to `pub(crate)` count as crate-visible;
/// `mod`/`use` are not raised, so a private one is invisible to the re-export.
fn item_export_vis(chunk_text: &str) -> ExportVis {
    use syn::{Item, Visibility};
    let Ok(item) = syn::parse_str::<syn::Item>(chunk_text) else {
        // Unparseable: bump_visibility also left it private, so a glob re-export
        // could not see it either — contribute nothing.
        return ExportVis::None;
    };
    let (vis, bumped) = match &item {
        Item::Fn(f) => (&f.vis, true),
        Item::Struct(s) => (&s.vis, true),
        Item::Enum(e) => (&e.vis, true),
        Item::Const(c) => (&c.vis, true),
        Item::Static(s) => (&s.vis, true),
        Item::Trait(t) => (&t.vis, true),
        Item::TraitAlias(t) => (&t.vis, true),
        Item::Type(t) => (&t.vis, true),
        Item::Union(u) => (&u.vis, true),
        Item::Mod(m) => (&m.vis, false),
        Item::Use(u) => (&u.vis, false),
        // impl / macro / foreign_mod / extern_crate / verbatim: nothing nameable.
        _ => return ExportVis::None,
    };
    match vis {
        Visibility::Public(_) => ExportVis::Pub,
        Visibility::Restricted(_) => ExportVis::Crate,
        Visibility::Inherited if bumped => ExportVis::Crate,
        Visibility::Inherited => ExportVis::None,
    }
}

/// The identifiers a set of moved items references — the basis for copying only
/// the imports they use. An item that does not re-parse flips `keep_all`, so the
/// caller falls back to copying every import rather than dropping a needed one.
#[derive(Default)]
struct Refs {
    names: std::collections::BTreeSet<String>,
    keep_all: bool,
}

impl Refs {
    fn add(&mut self, chunk_text: &str) {
        match crate::referenced_idents(chunk_text) {
            Some(names) => self.names.extend(names),
            None => self.keep_all = true,
        }
    }

    /// Add every identifier a multi-item source fragment (an extracted module
    /// body) references.
    fn add_source(&mut self, src: &str) {
        match crate::referenced_idents_in_source(src) {
            Some(names) => self.names.extend(names),
            None => self.keep_all = true,
        }
    }

    /// True when nothing was added — no import can be needed.
    fn is_empty(&self) -> bool {
        self.names.is_empty() && !self.keep_all
    }

    /// Whether an import providing `names` should be copied in. A name-less
    /// import (`None`: a `*` glob or `as _`) is always kept, since its usage
    /// can't be detected by name.
    fn keeps(&self, provided: &Option<std::collections::BTreeSet<String>>) -> bool {
        match provided {
            None => true,
            Some(names) => self.keep_all || names.iter().any(|n| self.names.contains(n)),
        }
    }
}

/// How a copied `use` path must be re-anchored, given where the copy lands
/// relative to the module the import was written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rebase {
    /// Same module — the paths already mean what they meant (the root file, and
    /// any body extracted to the module level it already had).
    Keep,
    /// One module level below a nested module: `super::` needs one more
    /// `super::`, and `self::` (the old module) becomes `super::`.
    Nested,
    /// One module level below a crate root: `self::` becomes `crate::`. A
    /// crate root has no `super::`, so nothing else can move.
    CrateRoot,
}

/// Re-anchor a copied `use` chunk for a file one module level below the one it
/// was written in. Only the leading segment moves: `crate::`, `::`-rooted and
/// external paths are untouched, and a re-export (`pub use …`) is never
/// rewritten. Span-driven on the re-parsed chunk, so leading comments,
/// attributes and formatting survive byte-exactly.
fn rebase_import(chunk_text: &str, rebase: Rebase) -> String {
    if matches!(rebase, Rebase::Keep) {
        return chunk_text.to_owned();
    }
    let Ok(syn::Item::Use(item)) = syn::parse_str::<syn::Item>(chunk_text) else {
        return chunk_text.to_owned();
    };
    // A re-export is the module's public surface and stays at the root verbatim;
    // a `::`-rooted path is already absolute.
    if !is_inherited(&item.vis) || item.leading_colon.is_some() {
        return chunk_text.to_owned();
    }
    let mut edits = Vec::new();
    collect_rebase_edits(&item.tree, rebase, &mut edits);
    apply_replacements(chunk_text, &edits)
}

/// Collect the byte range and replacement text for each leading `super`/`self`
/// segment of a `use` tree. A top-level group (`use {super::a, self::b};`)
/// anchors each of its items separately, so it recurses; a group *after* a path
/// segment does not (`use super::{self, a};` is anchored by the `super`).
fn collect_rebase_edits(
    tree: &syn::UseTree,
    rebase: Rebase,
    edits: &mut Vec<(std::ops::Range<usize>, &'static str)>,
) {
    match tree {
        syn::UseTree::Path(path) => collect_anchor_edit(&path.ident, rebase, edits),
        syn::UseTree::Rename(rename) => collect_anchor_edit(&rename.ident, rebase, edits),
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_rebase_edits(item, rebase, edits);
            }
        }
        _ => {}
    }
}

fn collect_anchor_edit(
    ident: &syn::Ident,
    rebase: Rebase,
    edits: &mut Vec<(std::ops::Range<usize>, &'static str)>,
) {
    let replacement = match (ident.to_string().as_str(), rebase) {
        ("super", Rebase::Nested) => "super::super",
        ("self", Rebase::Nested) => "super",
        ("self", Rebase::CrateRoot) => "crate",
        _ => return,
    };
    edits.push((ident.span().byte_range(), replacement));
}

/// Replace the given (non-overlapping, possibly empty) byte ranges in `text`.
/// An empty range is an insertion point.
fn apply_replacements(text: &str, edits: &[(std::ops::Range<usize>, &str)]) -> String {
    let mut edits: Vec<(std::ops::Range<usize>, &str)> = edits.to_vec();
    edits.sort_by_key(|(range, _)| range.start);
    let mut out = String::with_capacity(text.len() + edits.len() * 8);
    let mut prev = 0;
    for (range, replacement) in &edits {
        if range.start < prev {
            continue;
        }
        out.push_str(&text[prev..range.start]);
        out.push_str(replacement);
        prev = range.end;
    }
    out.push_str(&text[prev..]);
    out
}

/// Re-anchor the `super::` / `self::` paths *inside* a moved item — expression
/// and type paths, function-local `use` items, and macro invocation paths — for
/// a body that lands one module level below its original file. Attribute and
/// arbitrary macro input tokens are opaque; arguments to common standard
/// expression macros are parsed before their paths are rewritten. Comments,
/// strings, receivers, and visibility restrictions stay byte-exact.
fn rebase_body_paths(body: &str, rebase: Rebase) -> String {
    if matches!(rebase, Rebase::Keep) {
        return body.to_owned();
    }
    let Ok(file) = syn::parse_file(body) else {
        return body.to_owned();
    };
    let mut visitor = BodyPathEdits {
        rebase,
        edits: Vec::new(),
    };
    syn::visit::Visit::visit_file(&mut visitor, &file);
    apply_replacements(body, &visitor.edits)
}

struct BodyPathEdits {
    rebase: Rebase,
    edits: Vec<(std::ops::Range<usize>, &'static str)>,
}

impl<'ast> syn::visit::Visit<'ast> for BodyPathEdits {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        if path.leading_colon.is_none() && path.segments.len() > 1 {
            collect_anchor_edit(&path.segments[0].ident, self.rebase, &mut self.edits);
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if item.leading_colon.is_none() {
            collect_rebase_edits(&item.tree, self.rebase, &mut self.edits);
        }
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        syn::visit::Visit::visit_path(self, &mac.path);
        let Some(name) = mac.path.segments.last().map(|segment| &segment.ident) else {
            return;
        };
        let is_standard_path = mac.path.segments.len() == 1
            || (mac.path.segments.len() == 2
                && mac
                    .path
                    .segments
                    .first()
                    .is_some_and(|segment| segment.ident == "std" || segment.ident == "core"));
        if !is_standard_path
            || !matches!(
                name.to_string().as_str(),
                "assert"
                    | "assert_eq"
                    | "assert_ne"
                    | "debug_assert"
                    | "debug_assert_eq"
                    | "debug_assert_ne"
                    | "format"
                    | "format_args"
                    | "format_args_nl"
                    | "panic"
                    | "print"
                    | "println"
                    | "eprint"
                    | "eprintln"
                    | "write"
                    | "writeln"
            )
        {
            return;
        }
        // These standard macros accept comma-separated Rust expressions.
        // Unknown macro grammars remain opaque even when their tokens look Rust-like.
        let parser = syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated;
        let Ok(arguments) = parser.parse2(mac.tokens.clone()) else {
            return;
        };
        for argument in &arguments {
            syn::visit::Visit::visit_expr(self, argument);
        }
    }

    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}

    fn visit_visibility(&mut self, _: &'ast syn::Visibility) {}
}

/// Concatenate, in source order, the import chunks referenced by `refs` (plus
/// name-less imports like `*` globs and `as _`, always kept), each re-anchored
/// for where it lands. This is the aggressive policy: an import whose every name
/// is unreferenced is dropped — including an extension trait used only via its
/// methods, whose `use` the compiler will then ask to be re-added.
fn select_imports(
    import_chunks: &[usize],
    chunks: &[crate::Chunk],
    refs: &Refs,
    rebase: Rebase,
) -> String {
    let mut out = String::new();
    for &i in import_chunks {
        let text = chunks[i].text.as_str();
        if refs.keeps(&import_provided_names(text)) {
            out.push_str(&rebase_import(text, rebase));
        }
    }
    out
}

/// The names a `use`/`extern crate` chunk introduces, or `None` when it exposes
/// nothing referenceable by name — a `*` glob or an `as _` import — which is
/// then kept in every file rather than dropped.
fn import_provided_names(chunk_text: &str) -> Option<std::collections::BTreeSet<String>> {
    match syn::parse_str::<syn::Item>(chunk_text).ok()? {
        syn::Item::Use(u) => {
            let mut names = std::collections::BTreeSet::new();
            let mut name_less = false;
            collect_use_names(&u.tree, &mut names, &mut name_less);
            if name_less { None } else { Some(names) }
        }
        syn::Item::ExternCrate(c) => {
            let name = c
                .rename
                .map(|(_, id)| id.to_string())
                .unwrap_or_else(|| c.ident.to_string());
            Some(std::collections::BTreeSet::from([name]))
        }
        // An unparseable header chunk has no detectable name -> keep it.
        _ => None,
    }
}

/// Collect the bound names a `use` tree introduces. `name_less` is set for a
/// `*` glob or an `as _` rename, which bind no referenceable name.
fn collect_use_names(
    tree: &syn::UseTree,
    names: &mut std::collections::BTreeSet<String>,
    name_less: &mut bool,
) {
    match tree {
        syn::UseTree::Path(p) => collect_use_names(&p.tree, names, name_less),
        syn::UseTree::Name(n) => {
            names.insert(n.ident.to_string());
        }
        syn::UseTree::Rename(r) => {
            if r.rename == "_" {
                *name_less = true;
            } else {
                names.insert(r.rename.to_string());
            }
        }
        syn::UseTree::Glob(_) => *name_less = true,
        syn::UseTree::Group(g) => {
            for item in &g.items {
                collect_use_names(item, names, name_less);
            }
        }
    }
}

/// Split a `mod NAME { ... }` chunk into the root-side declaration and the
/// unwrapped inner body for the extracted file. The declaration is everything
/// up to the mod's identifier — leading comments, attributes (a `#[cfg(test)]`
/// gate must keep gating the declaration), and visibility (`pub mod` stays
/// `pub mod`) — closed with `;`. Span-driven on the re-parsed chunk, so it is
/// exact where the old line-based scan dropped the visibility. `None` when the
/// chunk does not re-parse as a body-carrying mod.
fn extract_mod(chunk_text: &str) -> Option<(String, String)> {
    let Ok(syn::Item::Mod(item)) = syn::parse_str::<syn::Item>(chunk_text) else {
        return None;
    };
    let (brace, _) = item.content.as_ref()?;
    let ident_end = item.ident.span().byte_range().end;
    let decl = format!("{};\n", &chunk_text[..ident_end]);

    let open_end = brace.span.open().byte_range().end;
    let close_start = brace.span.close().byte_range().start;
    let inner = &chunk_text[open_end..close_start];
    // Drop the newline that followed `{` and the closing brace's indentation.
    let inner = inner.strip_prefix('\n').unwrap_or(inner);
    let mut inner = inner.trim_end_matches([' ', '\t']).to_owned();
    if !inner.is_empty() && !inner.ends_with('\n') {
        inner.push('\n');
    }
    Some((decl, inner))
}

/// Recursively split an over-budget module's inner body into a `{name}/`
/// subdirectory: `{name}/mod.rs` forwards the enclosing module's items
/// (`pub(crate) use super::*`), keeps the body's own re-exports and `mod`
/// declarations verbatim, and re-exports the clusters; each `{name}/gNN.rs`
/// holds a cohesive cluster and reaches everything via `super::*`. A nested
/// body-carrying mod is extracted whole (attributes and visibility on its
/// declaration), recursing when it is itself over the ceiling. Used for big
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
    let row = |i: usize| {
        exploded
            .manifest
            .rows
            .iter()
            .find(|r| r.chunk_index == i)
            .unwrap()
    };

    // Budget from a conservative header estimate (every use/preamble/extern
    // chunk), then plan once. The copied group header is the plan's header —
    // the module's own plain imports — minus `use super::*` (the parent glob,
    // which `{name}/mod.rs` forwards instead). Re-exports and `mod`
    // declarations are in `root_items`, kept in `{name}/mod.rs`; a nested
    // `mod x;` still resolves to `{name}/x.rs` after the move.
    let header_estimate: usize = exploded
        .manifest
        .rows
        .iter()
        .filter(|r| matches!(r.kind.as_str(), "use" | "preamble" | "extern_crate"))
        .map(|r| r.loc)
        .sum();
    let item_budget = max_loc
        .saturating_sub(header_estimate + PART_OVERHEAD)
        .max(1);
    let plan = plan_split(&exploded, item_budget);
    let is_super_glob = |t: &str| t.replace(' ', "").contains("usesuper::*;");
    // `{name}/gNN.rs` is a module below `{name}`, so the body's own imports are
    // re-anchored the same way a part's are.
    let group_header: String = plan
        .header
        .iter()
        .map(|&i| exploded.chunks[i].text.as_str())
        .filter(|t| !is_super_glob(t))
        .map(|t| rebase_import(t, Rebase::Nested))
        .collect();

    let mut files = Vec::new();
    let mut decls = String::new();
    let mut reexports = String::new();
    let mut still = Vec::new();
    for (idx, part) in plan.parts.iter().enumerate() {
        let module = format!("g{idx:02}");
        let mut body = String::new();
        for &ci in &part.chunk_indices {
            body.push_str(&rebase_body_paths(
                &bump_visibility(exploded.chunks[ci].text.as_str()),
                Rebase::Nested,
            ));
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
    for &mod_index in &plan.mods {
        let chunk = exploded.chunks[mod_index].text.as_str();
        let mod_name = row(mod_index).name.as_str();
        let Some((decl, mod_inner)) = extract_mod(chunk) else {
            decls.push_str(chunk);
            continue;
        };
        decls.push_str(&decl);
        let mod_loc = loc(&mod_inner);
        if mod_loc < max_loc {
            files.push(OutputFile {
                path: format!("{name}/{mod_name}.rs"),
                contents: mod_inner,
                loc: mod_loc,
            });
        } else {
            let (nested, nested_still) = split_nested_mod(mod_name, &mod_inner, max_loc);
            files.extend(nested.into_iter().map(|mut f| {
                f.path = format!("{name}/{}", f.path);
                f
            }));
            still.extend(
                nested_still
                    .into_iter()
                    .map(|message| format!("{name}::{message}")),
            );
        }
    }
    for over in &plan.oversized {
        still.push(format!(
            "{name}::{} ({} LOC {}, leaf needs manual extraction)",
            over.name, over.loc, over.kind
        ));
    }

    let root_kept: String = plan
        .root_items
        .iter()
        .map(|&i| exploded.chunks[i].text.as_str())
        .collect();
    let modrs = format!("pub(crate) use super::*;\n\n{root_kept}{decls}\n{reexports}");
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
            !root.contents.contains("pub(crate) use "),
            "a single `pub use *` caps each item at its own visibility — no \
             redundant second `pub(crate) use` line"
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

    // ----- issue 1: a plain `foo.rs` file module places sub-modules in `foo/` ---

    #[test]
    fn file_module_places_submodules_in_a_subdir() {
        // A plain `foo.rs` file module (stem != "mod"): `mod bar;` in it resolves
        // to `foo/bar.rs`, so parts must be written under a `foo/` subdir, not as
        // siblings of `foo.rs` (which would not compile — E0583).
        let mut src = String::new();
        for k in 0..3 {
            // three items that reference nothing -> three separate parts
            src.push_str(&format!("pub fn item{k}() -> i32 {{\n    {k}\n}}\n\n"));
        }
        let exploded = explode(&src).unwrap();
        let out = split_mod(&exploded, 10_000, "wstest");

        let root = out.files.iter().find(|f| f.path == "wstest.rs").unwrap();
        assert!(
            root.contents.contains("mod item0;"),
            "root file-module declares the sub-modules"
        );
        let subs: Vec<&str> = out
            .files
            .iter()
            .map(|f| f.path.as_str())
            .filter(|p| *p != "wstest.rs")
            .collect();
        assert!(!subs.is_empty(), "produced sub-module files");
        for path in &subs {
            assert!(
                path.starts_with("wstest/"),
                "sub-module {path} must live under wstest/ (file-module subdir)"
            );
        }
    }

    #[test]
    fn mod_rs_keeps_submodules_as_siblings() {
        // The `foo/mod.rs` case (stem == "mod"): sub-modules stay siblings, no subdir.
        let mut src = String::new();
        for k in 0..3 {
            src.push_str(&format!("pub fn item{k}() -> i32 {{\n    {k}\n}}\n\n"));
        }
        let exploded = explode(&src).unwrap();
        let out = split_mod(&exploded, 10_000, "mod");
        for f in &out.files {
            assert!(
                !f.path.contains('/'),
                "mod.rs sub-modules are siblings, not in a subdir: {}",
                f.path
            );
        }
    }

    // ----- issue 2: per-module pub vs pub(crate) vs no re-export decision -------

    #[test]
    fn private_only_module_reexports_pub_crate_not_pub() {
        // No `pub` item: `pub use *` would warn "nothing public enough", so the
        // re-export must be `pub(crate) use`.
        let src = "fn helper() -> i32 {\n    leaf()\n}\n\nfn leaf() -> i32 {\n    1\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_mod(&exploded, 10_000, "mod");
        let root = out.files.iter().find(|f| f.path == "mod.rs").unwrap();
        assert!(
            root.contents.contains("pub(crate) use "),
            "all-private module re-exports pub(crate):\n{}",
            root.contents
        );
        assert!(
            !root.contents.contains("pub use "),
            "no `pub use` for a module with no public item"
        );
    }

    #[test]
    fn impl_only_module_is_declared_but_not_reexported() {
        // `api` is a normal part; the bare impl references no sibling, so it
        // clusters alone with nothing nameable -> declared, but no glob re-export
        // (re-exporting it would only warn "doesn't reexport anything").
        let src = "pub fn api() -> i32 {\n    1\n}\n\nimpl Worker for External {\n    fn run(&self) {}\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_mod(&exploded, 10_000, "mod");
        let root = out.files.iter().find(|f| f.path == "mod.rs").unwrap();

        // exactly one glob re-export line (for `api`) though two modules declared
        let reexports = root
            .contents
            .lines()
            .filter(|l| l.contains(" use ") && l.trim_end().ends_with("::*;"))
            .count();
        assert_eq!(
            reexports, 1,
            "only the nameable module is re-exported:\n{}",
            root.contents
        );
        assert!(root.contents.contains("pub use api::*;"));
        assert_eq!(
            root.contents.matches("mod ").count(),
            2,
            "both modules are still declared:\n{}",
            root.contents
        );
    }

    // ----- issue 3: crate-public entry fn is preserved as `pub use` -------------

    #[test]
    fn bin_public_entry_fn_is_reexported_pub_so_it_stays_crate_visible() {
        // A lib crate root where `run` is the public entry (the bin calls
        // `crate_name::run`). It must be re-exported `pub`, not `pub(crate)`, or
        // `crate_name::run` is no longer visible to main.rs after the split.
        let src = "fn helper() -> i32 {\n    1\n}\n\npub fn run() -> i32 {\n    helper()\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_bin(&exploded, 10_000, "lib");

        let root = out.files.iter().find(|f| f.path == "lib.rs").unwrap();
        assert!(
            root.contents.contains("pub use "),
            "public `run` re-exported at pub keeps `crate::run` visible:\n{}",
            root.contents
        );
        assert!(
            !root.contents.contains("pub(crate) use "),
            "the module has a pub item, so one `pub use *` suffices (no double line)"
        );
    }

    // ----- preamble (inner `//!`/`#![…]`) stays at the root, not copied to parts -

    #[test]
    fn crate_preamble_stays_at_root_not_copied_into_parts() {
        // `#![allow]` / `//!` belong at the crate root (and `#![feature]` is a
        // hard error anywhere else); a crate-root attr already covers sub-modules.
        let src = "//! crate docs\n#![allow(dead_code)]\n\npub fn alpha() -> i32 {\n    beta()\n}\n\nfn beta() -> i32 {\n    1\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_bin(&exploded, 10_000, "lib");

        let root = out.files.iter().find(|f| f.path == "lib.rs").unwrap();
        assert!(
            root.contents.contains("#![allow(dead_code)]"),
            "preamble stays at the root"
        );
        for f in &out.files {
            if f.path != "lib.rs" {
                assert!(
                    !f.contents.contains("#!["),
                    "part {} must not carry crate-level inner attrs",
                    f.path
                );
                assert!(
                    !f.contents.contains("//! crate docs"),
                    "part {} must not duplicate the crate docs",
                    f.path
                );
            }
        }
    }

    // ----- issue 3a: copy only the imports each file actually references -------

    #[test]
    fn parts_copy_only_the_imports_they_reference() {
        // alpha names `Display`; bravo names nothing from the header. Each part
        // gets only the imports it references.
        let src = "use std::fmt::Display;\nuse std::collections::HashMap;\n\npub fn alpha(d: &dyn Display) {\n    let _ = d;\n}\n\npub fn bravo() -> i32 {\n    1\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_mod(&exploded, 10_000, "mod");

        let alpha = out
            .files
            .iter()
            .find(|f| f.contents.contains("fn alpha"))
            .unwrap();
        let bravo = out
            .files
            .iter()
            .find(|f| f.contents.contains("fn bravo"))
            .unwrap();
        assert!(
            alpha.contents.contains("use std::fmt::Display;"),
            "alpha references Display:\n{}",
            alpha.contents
        );
        assert!(
            !alpha.contents.contains("HashMap"),
            "alpha never names HashMap -> import dropped:\n{}",
            alpha.contents
        );
        assert!(
            !bravo.contents.contains("use std::fmt::Display;"),
            "bravo references nothing from the header:\n{}",
            bravo.contents
        );
        assert!(!bravo.contents.contains("HashMap"));
    }

    #[test]
    fn globs_and_anonymous_trait_imports_are_always_copied() {
        // `use ...::*` and `use Trait as _` expose no name to match, so every
        // part keeps them — the safe choice for a trait used only via methods.
        let src = "use std::fmt::Write as _;\nuse std::prelude::v1::*;\n\npub fn writes(s: &mut String) {\n    let _ = write!(s, \"x\");\n}\n\npub fn other() -> i32 {\n    2\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_mod(&exploded, 10_000, "mod");
        for f in &out.files {
            if f.path == "mod.rs" {
                continue;
            }
            assert!(
                f.contents.contains("use std::fmt::Write as _;"),
                "{} must keep the `as _` trait import:\n{}",
                f.path,
                f.contents
            );
            assert!(
                f.contents.contains("use std::prelude::v1::*;"),
                "{} must keep the glob import:\n{}",
                f.path,
                f.contents
            );
        }
    }

    #[test]
    fn item_less_root_keeps_no_imports() {
        // A module root whose items all moved out is just decls + re-exports; it
        // needs no imports, while the part that uses one keeps it.
        let src = "use std::collections::HashMap;\n\npub fn solo(m: HashMap<u8, u8>) {\n    let _ = m;\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_mod(&exploded, 10_000, "mod");

        let root = out.files.iter().find(|f| f.path == "mod.rs").unwrap();
        assert!(
            !root.contents.contains("use std::collections::HashMap;"),
            "item-less root drops imports it doesn't use:\n{}",
            root.contents
        );
        let part = out.files.iter().find(|f| f.path != "mod.rs").unwrap();
        assert!(
            part.contents.contains("use std::collections::HashMap;"),
            "the part that uses HashMap keeps it"
        );
    }

    // ----- razel regression 1: source re-exports must stay `pub use` at root --

    #[test]
    fn source_pub_use_reexports_stay_pub_use_at_the_root() {
        // The original file's `pub use` lines ARE the crate's public API.
        // Treating them as droppable header imports (or demoting them to
        // `pub(crate) use`) silently breaks every dependent's paths.
        let src = "mod helpers;\n\npub use helpers::Helper;\n\nuse std::fmt::Display;\n\n\
                   pub fn api(d: &dyn Display) {\n    let _ = d;\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_bin(&exploded, 10_000, "lib");

        let root = out.files.iter().find(|f| f.path == "lib.rs").unwrap();
        assert!(
            root.contents.contains("mod helpers;"),
            "content-less mod decl stays at the root (it binds helpers.rs):\n{}",
            root.contents
        );
        assert!(
            root.contents.contains("pub use helpers::Helper;"),
            "source re-export stays verbatim at the root:\n{}",
            root.contents
        );
        assert!(
            !root.contents.contains("pub(crate) use helpers"),
            "a source re-export must never be demoted"
        );
        for f in &out.files {
            if f.path != "lib.rs" {
                assert!(
                    !f.contents.contains("pub use helpers::Helper;"),
                    "{} must not carry the root's re-export",
                    f.path
                );
                assert!(
                    !f.contents.contains("mod helpers;"),
                    "{} must not carry the root's mod decl (would look for a \
                     sibling subdir file)",
                    f.path
                );
            }
        }
    }

    // ----- razel regression 2: the cfg(test) gate travels with the tests mod --

    #[test]
    fn under_budget_cfg_test_mod_is_extracted_whole_with_its_gate() {
        // The razel failure: the tests mod fit under the item budget, so it was
        // CLUSTERED — wrapped inside a part module with an ungated root decl —
        // instead of extracted. A body mod must always move whole, its
        // `#[cfg(test)]` gating the root declaration.
        let src = "pub fn alpha() -> i32 {\n    1\n}\n\n\
                   #[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    \
                   fn smoke() {\n        assert_eq!(alpha(), 1);\n    }\n}\n";
        let exploded = explode(src).unwrap();
        // Budget far above the tests mod's size: it would have been clustered.
        let out = split_bin(&exploded, 10_000, "lib");

        let root = out.files.iter().find(|f| f.path == "lib.rs").unwrap();
        assert!(
            root.contents.contains("#[cfg(test)]\nmod tests;"),
            "the gate must travel to the root declaration:\n{}",
            root.contents
        );
        assert!(
            !root.contents.contains("use tests::*"),
            "an extracted mod is declared, not glob re-exported:\n{}",
            root.contents
        );
        let tests = out.files.iter().find(|f| f.path == "tests.rs").unwrap();
        assert!(tests.contents.contains("fn smoke()"));
        assert!(
            !tests.contents.contains("mod tests"),
            "the body is unwrapped — no double nesting:\n{}",
            tests.contents
        );
    }

    #[test]
    fn pub_mod_keeps_its_visibility_on_the_extracted_decl() {
        let src = "pub fn api() -> i32 {\n    1\n}\n\n\
                   /// Config store.\npub mod config {\n    pub fn get() -> i32 {\n        2\n    }\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_bin(&exploded, 10_000, "lib");

        let root = out.files.iter().find(|f| f.path == "lib.rs").unwrap();
        assert!(
            root.contents.contains("/// Config store.\npub mod config;"),
            "docs and `pub` travel to the declaration:\n{}",
            root.contents
        );
        let config = out.files.iter().find(|f| f.path == "config.rs").unwrap();
        assert!(config.contents.contains("pub fn get()"));
    }

    #[test]
    fn part_colliding_with_an_extracted_mod_name_is_renamed() {
        // `fn tests` (value namespace) and `mod tests` (type namespace) can
        // coexist in the source; the extracted mod owns `tests.rs`, so the
        // part named after the fn must yield.
        let src = "fn tests() -> i32 {\n    1\n}\n\n\
                   #[cfg(test)]\nmod tests {\n    fn smoke() {}\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_bin(&exploded, 10_000, "lib");

        let paths: Vec<&str> = out.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"tests.rs"), "extracted mod file: {paths:?}");
        assert!(paths.contains(&"tests_.rs"), "renamed part file: {paths:?}");
        let root = out.files.iter().find(|f| f.path == "lib.rs").unwrap();
        assert!(root.contents.contains("mod tests_;"));
        assert!(root.contents.contains("#[cfg(test)]\nmod tests;"));
    }

    #[test]
    fn root_keeps_imports_an_extracted_mod_body_references() {
        // The extracted tests mod reaches the root's imports via
        // `use super::*;` — dropping HashMap from the root would break it.
        let src = "use std::collections::HashMap;\n\n\
                   pub fn alpha() -> i32 {\n    1\n}\n\n\
                   #[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    \
                   fn smoke() {\n        let _m: HashMap<u8, u8> = HashMap::new();\n        \
                   assert_eq!(alpha(), 1);\n    }\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_bin(&exploded, 10_000, "lib");

        let root = out.files.iter().find(|f| f.path == "lib.rs").unwrap();
        assert!(
            root.contents.contains("use std::collections::HashMap;"),
            "root must keep imports the extracted body names:\n{}",
            root.contents
        );
    }

    #[test]
    fn bin_root_keeps_only_imports_main_references() {
        // `main` names `exit` but not `HashMap` (the moved `worker` does), so the
        // root keeps `exit` and drops `HashMap`.
        let src = "use std::process::exit;\nuse std::collections::HashMap;\n\nfn worker() -> HashMap<u8, u8> {\n    HashMap::new()\n}\n\nfn main() {\n    let _ = worker();\n    exit(0);\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_bin(&exploded, 10_000, "main");

        let root = out.files.iter().find(|f| f.path == "main.rs").unwrap();
        assert!(
            root.contents.contains("use std::process::exit;"),
            "main references exit:\n{}",
            root.contents
        );
        assert!(
            !root.contents.contains("HashMap"),
            "main never names HashMap (worker does) -> dropped from root:\n{}",
            root.contents
        );
    }

    // ----- issue 4: a part sits one module level below the file it came from --

    #[test]
    fn nested_module_part_imports_gain_one_super_level() {
        // A part of `foo.rs` is module `foo::<part>`, one level below `foo`, so a
        // copied `use super::X` would name `foo` instead of `foo`'s parent and a
        // copied `use self::X` would name the part itself.
        let src = r#"use super::sibling::Thing;
use super::{alpha, beta::Gamma};
use self::local::X;
use super::*;
use crate::top::Absolute;
use std::fmt::Display;

pub fn one(t: Thing, g: Gamma, x: X, d: &dyn Display) -> i32 {
    let _: Option<Absolute> = None;
    let _ = (t, g, x, d);
    alpha()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke() {
        let _ = Thing;
    }
}
"#;
        let exploded = explode(src).unwrap();
        let out = split_mod(&exploded, 10_000, "foo");

        let part = out.files.iter().find(|f| f.path == "foo/one.rs").unwrap();
        for expected in [
            "use super::super::sibling::Thing;",
            "use super::super::{alpha, beta::Gamma};",
            "use super::local::X;",
            "use super::super::*;",
            // `crate::` and external paths are absolute — untouched.
            "use crate::top::Absolute;",
            "use std::fmt::Display;",
        ] {
            assert!(
                part.contents.contains(expected),
                "part must carry `{expected}`:\n{}",
                part.contents
            );
        }
        // The tool's own sibling glob is still there, exactly once, and is not
        // confused with the header's rewritten `use super::*;`.
        assert_eq!(
            part.contents
                .lines()
                .filter(|l| l.trim() == "use super::*;")
                .count(),
            1,
            "exactly one tool-added sibling glob:\n{}",
            part.contents
        );

        // The root is still the original module — its header must not move.
        let root = out.files.iter().find(|f| f.path == "foo.rs").unwrap();
        assert!(
            root.contents.contains("use super::sibling::Thing;"),
            "root header stays byte-unchanged:\n{}",
            root.contents
        );
        assert!(
            !root.contents.contains("super::super"),
            "the root did not move — nothing to re-anchor:\n{}",
            root.contents
        );
    }

    #[test]
    fn crate_root_part_imports_rewrite_self_to_crate() {
        // Below a crate root `self::X` names the part, not the crate root;
        // `super::` cannot occur at a crate root, so nothing else changes.
        let src = "use self::helpers::Helper;\nuse std::fmt::Display;\n\n\
                   pub fn api(h: Helper, d: &dyn Display) {\n    let _ = (h, d);\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_bin(&exploded, 10_000, "lib");

        let part = out.files.iter().find(|f| f.path != "lib.rs").unwrap();
        assert!(
            part.contents.contains("use crate::helpers::Helper;"),
            "`self::` re-anchors to `crate::` below a crate root:\n{}",
            part.contents
        );
        assert!(part.contents.contains("use std::fmt::Display;"));
    }

    #[test]
    fn nested_split_group_headers_gain_one_super_level() {
        // `tests/gNN.rs` is module `tests::gNN`, one below the extracted `tests`
        // body the group header was copied from.
        let mut inner = String::from(
            "    use super::outer::Thing;\n    use self::inner::Y;\n\n\
             \x20   fn helper() -> i32 {\n        1\n    }\n",
        );
        for i in 0..30 {
            inner.push_str(&format!(
                "\n    #[test]\n    fn t{i}() {{\n        \
                 let _: Option<Thing> = None;\n        let _: Option<Y> = None;\n        \
                 assert_eq!(helper(), 1);\n    }}\n"
            ));
        }
        let src = format!(
            "pub fn helper() -> i32 {{\n    1\n}}\n\nfn main() {{}}\n\n\
             #[cfg(test)]\nmod tests {{\n{inner}}}\n"
        );
        let exploded = explode(&src).unwrap();
        let out = split_bin(&exploded, 60, "main");

        let group = out.files.iter().find(|f| f.path == "tests/g00.rs").unwrap();
        assert!(
            group.contents.contains("use super::super::outer::Thing;"),
            "group header `super::` gains a level:\n{}",
            group.contents
        );
        assert!(
            group.contents.contains("use super::inner::Y;"),
            "group header `self::` becomes `super::`:\n{}",
            group.contents
        );
    }

    #[test]
    fn extracted_inline_mod_keeps_its_own_super_paths() {
        // An inline `mod tests` extracted out of `foo.rs` lands at `foo/tests.rs`
        // — still module `foo::tests`, the level it already had — so its own
        // `use super::helper;` still names `foo` and must NOT be re-anchored.
        let src = "pub fn helper() -> i32 {\n    1\n}\n\n\
                   #[cfg(test)]\nmod tests {\n    use super::helper;\n\n    #[test]\n    \
                   fn smoke() {\n        assert_eq!(helper(), 1);\n    }\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_mod(&exploded, 10_000, "foo");

        let tests = out.files.iter().find(|f| f.path == "foo/tests.rs").unwrap();
        assert!(
            tests.contents.contains("use super::helper;"),
            "an extracted mod keeps its own module level:\n{}",
            tests.contents
        );
        assert!(
            !tests.contents.contains("super::super"),
            "the extracted body did not change module level:\n{}",
            tests.contents
        );
    }

    // ----- issue 5: include! paths resolve against the containing file --------

    #[test]
    fn include_paths_in_moved_bodies_are_rebased_one_directory_deeper() {
        let src = r#"pub fn doc() -> &'static str {
    include_str!("../docs/a.md")
}

pub fn blob() -> &'static [u8] {
    include_bytes!("fixtures/b.bin")
}

pub fn built() -> &'static str {
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/x"))
}

pub fn rooted() -> &'static str {
    include_str!("/etc/x")
}
"#;
        let exploded = explode(src).unwrap();
        let out = split_mod(&exploded, 10_000, "foo");

        let moved: String = out
            .files
            .iter()
            .filter(|f| f.path != "foo.rs")
            .map(|f| f.contents.as_str())
            .collect();
        assert!(
            moved.contains(r#"include_str!("../../docs/a.md")"#),
            "a relative include! path gains one `../`:\n{moved}"
        );
        assert!(
            moved.contains(r#"include_bytes!("../fixtures/b.bin")"#),
            "a bare relative path is prefixed too:\n{moved}"
        );
        assert!(
            moved.contains(r#"include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/x"))"#),
            "a non-literal argument is byte-unchanged:\n{moved}"
        );
        assert!(
            moved.contains(r#"include_str!("/etc/x")"#),
            "an absolute path is byte-unchanged:\n{moved}"
        );
    }

    #[test]
    fn include_paths_are_untouched_when_the_file_stays_in_its_directory() {
        // A crate root's parts are siblings of the root file, so no body moved
        // between directories and no include! path may change.
        let src = "fn worker() -> &'static str {\n    include_str!(\"../docs/w.md\")\n}\n\n\
                   fn main() {\n    let _ = include_str!(\"../docs/m.md\");\n    \
                   let _ = worker();\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_bin(&exploded, 10_000, "main");

        for f in &out.files {
            assert!(
                !f.contents.contains("../../docs/"),
                "{} stayed in the source directory:\n{}",
                f.path,
                f.contents
            );
        }
        let root = out.files.iter().find(|f| f.path == "main.rs").unwrap();
        assert!(root.contents.contains(r#"include_str!("../docs/m.md")"#));
    }

    // ----- issue 4b: relative paths *inside* a moved body move with it --------

    #[test]
    fn review_body_paths_preserve_opaque_macro_and_attribute_tokens() {
        let src = r#"#[metadata(super::Thing)]
pub fn probe() -> super::Thing {
    macro_rules! data { () => { self::Thing }; }
    let _ = stringify!(super::Thing);
    let _ = stringify! { self::Thing };
    let _ = custom!(super::Thing, (self::Thing), stringify!(super::Thing));
    let _ = super::factory!(self::Thing);
    <super::Thing as self::Trait>::make()
}
"#;
        let expected = src
            .replace("-> super::Thing", "-> super::super::Thing")
            .replace("super::factory!", "super::super::factory!")
            .replace(
                "<super::Thing as self::Trait>",
                "<super::super::Thing as super::Trait>",
            );
        assert_eq!(rebase_body_paths(src, Rebase::Nested), expected);
        assert_eq!(rebase_body_paths(src, Rebase::Keep), src);
    }

    #[test]
    fn review_standard_expression_macro_arguments_rebase_paths() {
        let src = r#"fn probe() {
    assert_eq!(super::Thing::value(), self::value(), "{}", super::message());
    let _ = format!("{}", super::Thing::value());
    let _ = custom!(super::Thing, self::Thing);
    let _ = custom::assert_eq!(super::Thing, self::Thing);
    let _ = stringify!(super::Thing);
}
"#;
        let expected = src
            .replace("super::Thing::value()", "super::super::Thing::value()")
            .replace("self::value()", "super::value()")
            .replace("super::message()", "super::super::message()");
        assert_eq!(rebase_body_paths(src, Rebase::Nested), expected);
    }

    #[test]
    fn review_renamed_use_anchors_rebase_in_headers_and_bodies() {
        for (source, nested, root) in [
            (
                "super as parent",
                "super::super as parent",
                "super as parent",
            ),
            ("self as current", "super as current", "crate as current"),
            (
                "{super as parent, self as current, super::{self as p, Thing}}",
                "{super::super as parent, super as current, super::super::{self as p, Thing}}",
                "{super as parent, crate as current, super::{self as p, Thing}}",
            ),
            ("crate as root", "crate as root", "crate as root"),
        ] {
            for (rebase, expected) in [
                (Rebase::Nested, nested),
                (Rebase::CrateRoot, root),
                (Rebase::Keep, source),
            ] {
                let import =
                    format!("// keep this comment\n#[allow(unused_imports)]\nuse {source};\n");
                let expected = import.replace(source, expected);
                assert_eq!(rebase_import(&import, rebase), expected);
                assert_eq!(
                    rebase_body_paths(&format!("fn probe() {{\n{import}}}"), rebase),
                    format!("fn probe() {{\n{expected}}}"),
                );
            }
        }
    }

    #[test]
    fn review_include_literals_accept_one_optional_trailing_comma() {
        for name in PATH_MACROS {
            for literal in [r#""docs/file""#, r##"r#"docs/file"#"##] {
                let src = format!("fn probe() {{ {name}!({literal} /* keep */,); }}");
                assert_eq!(
                    rebase_includes(&src, 2, "foo/part.rs"),
                    src.replace("docs/file", "../../docs/file"),
                );
                for suffix in [",,", ", other", " + other"] {
                    let src = format!("fn probe() {{ {name}!({literal}{suffix}); }}");
                    assert_eq!(rebase_includes(&src, 1, "foo/part.rs"), src);
                }
            }
        }
    }

    #[test]
    fn review_include_absolute_windows_paths_are_host_independent() {
        for name in PATH_MACROS {
            for literal in [
                r#""C:\\docs\\file""#,
                r#""z:/docs/file""#,
                r#"r"C:\docs\file""#,
                r#""\\\\server\\share\\file""#,
                r#"r"\\server\share\file""#,
                r#"r"\\?\C:\docs\file""#,
                r#""//server/share/file""#,
                r#""/docs/file""#,
            ] {
                let src = format!("fn probe() {{ {name}!({literal}); }}");
                assert_eq!(rebase_includes(&src, 1, "foo/part.rs"), src);
            }
            let src = format!("fn probe() {{ {name}!(\"C:relative\"); }}");
            assert_eq!(
                rebase_includes(&src, 1, "foo/part.rs"),
                src.replace("C:relative", "../C:relative"),
            );
        }
    }

    #[test]
    fn moved_body_paths_gain_one_super_level() {
        // The header is not the only place a relative path hides: expression and
        // type paths, and a function-local `use`, travel inside the item.
        let src = r#"pub fn probe() -> i32 {
    use super::alpha::Beta;
    let _ = Beta;
    let _ = super::filter::parse("x");
    let _ = super::super::grandparent::thing();
    let _ = self::local::helper();
    let _: super::sibling::Kind = super::sibling::make();
    let _ = super::sibling::Record {
        meta: super::meta::build("req"),
    };
    crate::top::value()
}
"#;
        let exploded = explode(src).unwrap();
        let out = split_mod(&exploded, 10_000, "foo");

        let part = out.files.iter().find(|f| f.path == "foo/probe.rs").unwrap();
        for expected in [
            "use super::super::alpha::Beta;",
            r#"super::super::filter::parse("x")"#,
            // Only the leading segment moves: two levels become three, not four.
            "super::super::super::grandparent::thing()",
            "super::local::helper()",
            // A lone `:` — a type annotation, a struct field init — is not a
            // path separator, so the segment after it still starts a path.
            "let _: super::super::sibling::Kind = super::super::sibling::make();",
            "meta: super::super::meta::build(\"req\"),",
            "crate::top::value()",
        ] {
            assert!(
                part.contents.contains(expected),
                "moved body must carry `{expected}`:\n{}",
                part.contents
            );
        }
    }

    #[test]
    fn moved_body_leaves_self_receivers_and_use_groups_alone() {
        // `self` is only a module anchor when a `::` follows it: a receiver, a
        // `use …::{self, …}` group member and a `pub(super)` restriction are all
        // something else. (Re-anchoring a `pub(super)` restriction — which does
        // narrow when the item moves — is out of scope here.)
        let src = r#"pub struct Widget {
    pub n: i32,
}

impl Widget {
    pub fn get(&self) -> i32 {
        use std::collections::{self, HashMap};
        let _: Option<HashMap<u8, u8>> = None;
        let _ = collections::BTreeMap::<u8, u8>::new();
        let _ = Self::make();
        self.n
    }

    pub fn make() -> Self {
        Self { n: 1 }
    }
}

pub(super) fn gated() -> i32 {
    1
}
"#;
        let exploded = explode(src).unwrap();
        let out = split_mod(&exploded, 10_000, "foo");
        let moved: String = out
            .files
            .iter()
            .filter(|f| f.path != "foo.rs")
            .map(|f| f.contents.as_str())
            .collect();

        for expected in [
            "use std::collections::{self, HashMap};",
            "Self::make()",
            "self.n",
            "pub(super) fn gated",
        ] {
            assert!(
                moved.contains(expected),
                "`{expected}` is not a module anchor and must not move:\n{moved}"
            );
        }
        assert!(
            !moved.contains("pub(super::super)"),
            "a `pub(super)` restriction must never be mangled:\n{moved}"
        );
    }

    #[test]
    fn moved_body_paths_below_a_crate_root_rewrite_self_to_crate() {
        let src = "pub fn entry() -> i32 {\n    let _ = self::helpers::setup();\n    \
                   crate::other::value()\n}\n";
        let exploded = explode(src).unwrap();
        let out = split_bin(&exploded, 10_000, "lib");

        let part = out.files.iter().find(|f| f.path != "lib.rs").unwrap();
        assert!(
            part.contents.contains("crate::helpers::setup()"),
            "`self::` below a crate root is `crate::`:\n{}",
            part.contents
        );
        assert!(part.contents.contains("crate::other::value()"));
    }

    #[test]
    fn explode_never_rewrites_include_paths() {
        // Re-basing belongs to `split`; `explode` stays byte-exact.
        let src = "pub fn doc() -> &'static str {\n    include_str!(\"../docs/a.md\")\n}\n";
        let exploded = explode(src).unwrap();
        let joined: String = exploded.chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined, src, "explode must stay lossless");
    }
}
