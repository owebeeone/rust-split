//! Tier 2 step 1 — the cohesion clusterer.
//!
//! This is **not** a bin-packer. The objective is to put **related items
//! together and unrelated items apart**. The number of output files is a
//! *consequence* of how related the code is, never something to minimise: if a
//! file genuinely contains ten unrelated 50-line items, ten 50-line files is the
//! correct answer, and merging any of them to "save a file" or "fill a bin"
//! makes the split worse. `max_loc` is only a **ceiling**, never a packing
//! target: a transitively related group bigger than **half** the ceiling is
//! partitioned into roughly equal cohesive parts (strongest reference edges
//! bond first, so the cuts fall on the weakest edges), leaning toward *more*
//! files rather than files that graze the ceiling.
//!
//! Relation: an item references a sibling (the manifest's `adjacency_hint`).
//! Clustering is greedy agglomerative on those reference edges, heaviest edge
//! first, capped at the component's balanced share; leftover clusters of the
//! same component are then consolidated in source order up to the same share.
//! Items with no edges stay alone. Imports/preamble are pulled out as a shared
//! header (the reassembler copies into each module only the imports it
//! references); an item that is itself `>= max_loc` is reported oversized.
//!
//! Three chunk classes never enter clustering:
//! - visibility-qualified `use`/`extern crate` (`pub use ...`) are **re-exports**
//!   — the file's API surface — and stay at the root verbatim (`root_items`);
//! - content-less `mod name;` declarations bind files relative to the root's
//!   directory and stay at the root verbatim (`root_items`);
//! - body-carrying `mod name { ... }` items are already module boundaries and
//!   are extracted whole to their own file (`mods`), the wrapper's attributes
//!   (e.g. `#[cfg(test)]`) and visibility traveling to the root declaration.

use crate::Exploded;
use std::collections::{BTreeMap, HashMap};

/// The result of planning a split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPlan {
    /// Chunk indices forming the shared header (preamble + plain imports).
    pub header: Vec<usize>,
    /// Chunk indices kept at the root verbatim, in source order: re-exports
    /// (`pub use ...`, `pub(crate) use ...`, visibility-qualified
    /// `extern crate`) and content-less `mod name;` declarations. Dropping or
    /// demoting a `pub use` would silently narrow the crate's public API.
    pub root_items: Vec<usize>,
    /// Chunk indices of body-carrying `mod name { ... }` items, in source
    /// order. Each is extracted whole to its own file regardless of size —
    /// never packed into a cluster, so its `#[cfg(test)]`-style gate and
    /// visibility can travel to the root declaration.
    pub mods: Vec<usize>,
    /// Cohesive destination modules, in source order.
    pub parts: Vec<Part>,
    /// Items that cannot be placed under the ceiling on their own.
    pub oversized: Vec<Oversized>,
}

/// One cohesive module: a cluster of related items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// A content-derived module name (the cluster's dominant item).
    pub name: String,
    /// Chunk indices packed here, in source order.
    pub chunk_indices: Vec<usize>,
    /// Total LOC; always `< max_loc`.
    pub loc: usize,
}

/// An item whose own LOC is at or above the ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Oversized {
    pub chunk_index: usize,
    pub name: String,
    pub kind: String,
    pub loc: usize,
    /// True for a `mod` (recoverable by a nested split); false for a leaf.
    /// Body-carrying mods are now planned via `mods` (any size), so an
    /// oversized entry is a leaf and this is false unless the mod chunk failed
    /// to re-parse.
    pub recoverable: bool,
}

impl SplitPlan {
    pub fn is_complete(&self) -> bool {
        self.oversized.is_empty()
    }
}

struct Item {
    chunk_index: usize,
    name: String,
    kind: String,
    loc: usize,
    refs: Vec<String>,
}

/// Cluster `exploded`'s items into cohesive modules, each `< max_loc`.
pub fn plan_split(exploded: &Exploded, max_loc: usize) -> SplitPlan {
    let mut header = Vec::new();
    let mut root_items = Vec::new();
    let mut mods = Vec::new();
    let mut oversized = Vec::new();
    let mut items: Vec<Item> = Vec::new();

    for row in &exploded.manifest.rows {
        let chunk_text = exploded.chunks[row.chunk_index].text.as_str();
        match row.kind.as_str() {
            "preamble" => header.push(row.chunk_index),
            "use" | "extern_crate" => {
                if is_reexport(chunk_text) {
                    root_items.push(row.chunk_index);
                } else {
                    header.push(row.chunk_index);
                }
            }
            "mod" => match mod_body(chunk_text) {
                Some(true) => mods.push(row.chunk_index),
                Some(false) => root_items.push(row.chunk_index),
                // Chunk did not re-parse: fall back to clustering it intact.
                None => items.push(Item {
                    chunk_index: row.chunk_index,
                    name: row.name.clone(),
                    kind: row.kind.clone(),
                    loc: row.loc,
                    refs: row.adjacency_hint.clone(),
                }),
            },
            _ if row.loc >= max_loc => oversized.push(Oversized {
                chunk_index: row.chunk_index,
                name: row.name.clone(),
                kind: row.kind.clone(),
                loc: row.loc,
                recoverable: row.kind == "mod",
            }),
            _ => items.push(Item {
                chunk_index: row.chunk_index,
                name: row.name.clone(),
                kind: row.kind.clone(),
                loc: row.loc,
                refs: row.adjacency_hint.clone(),
            }),
        }
    }

    // An unparseable mod chunk that is itself over the ceiling still needs
    // reporting rather than forcing.
    items.retain(|item| {
        if item.loc >= max_loc {
            oversized.push(Oversized {
                chunk_index: item.chunk_index,
                name: item.name.clone(),
                kind: item.kind.clone(),
                loc: item.loc,
                recoverable: item.kind == "mod",
            });
            false
        } else {
            true
        }
    });
    oversized.sort_by_key(|over| over.chunk_index);

    let parts = if items.is_empty() {
        Vec::new()
    } else {
        cluster(&items, max_loc)
    };
    SplitPlan {
        header,
        root_items,
        mods,
        parts,
        oversized,
    }
}

/// Whether a `use`/`extern crate` chunk is a **re-export** (visibility-
/// qualified: `pub`, `pub(crate)`, `pub(super)`, `pub(in ...)`). Re-exports are
/// part of the file's API surface and must stay at the root verbatim; a plain
/// private import is a header chunk copied into the files that reference it.
fn is_reexport(chunk_text: &str) -> bool {
    match syn::parse_str::<syn::Item>(chunk_text) {
        Ok(syn::Item::Use(item)) => !matches!(item.vis, syn::Visibility::Inherited),
        Ok(syn::Item::ExternCrate(item)) => !matches!(item.vis, syn::Visibility::Inherited),
        // Unparseable or unexpected: keeping it in the copied header is the
        // conservative choice (nothing is dropped from the root's own uses).
        _ => false,
    }
}

/// `Some(true)` for a body-carrying `mod name { ... }`, `Some(false)` for a
/// content-less `mod name;` declaration, `None` when the chunk does not
/// re-parse as a mod.
fn mod_body(chunk_text: &str) -> Option<bool> {
    match syn::parse_str::<syn::Item>(chunk_text) {
        Ok(syn::Item::Mod(item)) => Some(item.content.is_some()),
        _ => None,
    }
}

/// Greedy agglomerative clustering on reference edges.
///
/// The ceiling is not a packing target: each connected component (the set of
/// transitively related items) gets a **balanced share** — a component whose
/// total fits within half the ceiling becomes one part; a bigger one is
/// partitioned into `ceil(total / (max_loc / 2))` roughly equal parts. Merging
/// is heaviest-edge-first up to that share, so the strongest relations bond
/// first and the cuts fall on the weakest edges; same-component leftovers are
/// then consolidated in source order up to the same share. Unrelated items
/// (separate components) are never merged.
fn cluster(items: &[Item], max_loc: usize) -> Vec<Part> {
    let n = items.len();

    // name -> item index (first occurrence; duplicate top-level names are rare)
    let mut name_to_item: HashMap<&str, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        name_to_item.entry(item.name.as_str()).or_insert(i);
    }

    // weighted undirected reference edges
    let mut weights: HashMap<(usize, usize), usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        for reference in &item.refs {
            if let Some(&j) = name_to_item.get(reference.as_str())
                && i != j
            {
                *weights.entry((i.min(j), i.max(j))).or_insert(0) += 1;
            }
        }
    }
    let mut edges: Vec<((usize, usize), usize)> = weights.into_iter().collect();
    // heaviest edge first; ties by source position for determinism
    edges.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    // Connected components over all edges, then each component's balanced
    // per-part share.
    let mut component: Vec<usize> = (0..n).collect();
    for &((i, j), _weight) in &edges {
        let ri = find(&mut component, i);
        let rj = find(&mut component, j);
        if ri != rj {
            component[rj] = ri;
        }
    }
    let mut component_loc: HashMap<usize, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        let root = find(&mut component, i);
        *component_loc.entry(root).or_insert(0) += item.loc;
    }
    let target = (max_loc / 2).max(1);
    let share = |component_root: usize, component_loc: &HashMap<usize, usize>| -> usize {
        let total = component_loc[&component_root];
        if total <= target {
            total
        } else {
            total.div_ceil(total.div_ceil(target))
        }
    };

    let mut parent: Vec<usize> = (0..n).collect();
    let mut group_loc: Vec<usize> = items.iter().map(|item| item.loc).collect();
    for ((i, j), _weight) in edges {
        let cap = share(find(&mut component, i), &component_loc);
        let ri = find(&mut parent, i);
        let rj = find(&mut parent, j);
        if ri != rj && group_loc[ri] + group_loc[rj] <= cap {
            parent[rj] = ri;
            group_loc[ri] += group_loc[rj];
        }
    }

    // Consolidate leftovers: clusters of the same component that the capped
    // edge pass left apart (e.g. the satellites of an already-full hub) are
    // still transitively related, so pack them in source order up to the same
    // share rather than emitting a file per satellite.
    let mut first_chunk: HashMap<usize, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        let root = find(&mut parent, i);
        let first = first_chunk.entry(root).or_insert(usize::MAX);
        *first = (*first).min(item.chunk_index);
    }
    let mut by_component: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        if find(&mut parent, i) == i {
            let component_root = find(&mut component, i);
            by_component.entry(component_root).or_default().push(i);
        }
    }
    for (component_root, mut cluster_roots) in by_component {
        let cap = share(component_root, &component_loc);
        cluster_roots.sort_by_key(|root| first_chunk[root]);
        let mut accumulator: Option<usize> = None;
        for root in cluster_roots {
            match accumulator {
                Some(acc) if group_loc[acc] + group_loc[root] <= cap => {
                    parent[root] = acc;
                    group_loc[acc] += group_loc[root];
                }
                _ => accumulator = Some(root),
            }
        }
    }

    // collect members per root
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }

    let mut parts: Vec<Part> = groups
        .into_values()
        .map(|members| {
            let mut chunk_indices: Vec<usize> =
                members.iter().map(|&i| items[i].chunk_index).collect();
            chunk_indices.sort_unstable();
            let loc = members.iter().map(|&i| items[i].loc).sum();
            Part {
                name: cluster_name(&members, items),
                chunk_indices,
                loc,
            }
        })
        .collect();

    parts.sort_by_key(|part| part.chunk_indices.first().copied().unwrap_or(0));
    dedupe_names(&mut parts);
    parts
}

fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]]; // path halving
        x = parent[x];
    }
    x
}

/// Name a cluster after its largest nameable item (skipping `impl`/`use`).
fn cluster_name(members: &[usize], items: &[Item]) -> String {
    let nameable = |kind: &str| {
        matches!(
            kind,
            "fn" | "struct"
                | "enum"
                | "const"
                | "static"
                | "trait"
                | "trait_alias"
                | "type"
                | "union"
                | "mod"
        )
    };
    let dominant = members
        .iter()
        .filter(|&&i| nameable(&items[i].kind))
        .max_by_key(|&&i| items[i].loc)
        .or_else(|| members.iter().max_by_key(|&&i| items[i].loc));
    match dominant {
        Some(&i) => sanitize(&items[i].name),
        None => "group".to_owned(),
    }
}

fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    let out = out.trim_matches('_').to_owned();
    if out.is_empty() || out.chars().next().unwrap().is_ascii_digit() {
        format!("m_{out}")
    } else {
        out
    }
}

fn dedupe_names(parts: &mut [Part]) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for part in parts.iter_mut() {
        let count = seen.entry(part.name.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            part.name = format!("{}_{}", part.name, *count);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explode;

    fn fns(defs: &[(&str, &[&str])]) -> String {
        // each (name, calls) -> a fn that calls the named siblings
        let mut src = String::new();
        for (name, calls) in defs {
            src.push_str(&format!("fn {name}() {{\n"));
            for c in *calls {
                src.push_str(&format!("    {c}();\n"));
            }
            src.push_str("}\n\n");
        }
        src
    }

    #[test]
    fn unrelated_items_become_separate_files_not_merged() {
        // ten items that reference nothing -> ten clusters (NOT fewer)
        let src = fns(&[
            ("a", &[]),
            ("b", &[]),
            ("c", &[]),
            ("d", &[]),
            ("e", &[]),
            ("f", &[]),
            ("g", &[]),
            ("h", &[]),
            ("i", &[]),
            ("j", &[]),
        ]);
        let exploded = explode(&src).unwrap();
        // budget is huge: they would all fit in one file, but they are unrelated
        let plan = plan_split(&exploded, 10_000);
        assert_eq!(plan.parts.len(), 10, "unrelated items must not be merged");
    }

    #[test]
    fn related_items_cluster_together_unrelated_stay_apart() {
        // group A: a1->a2->a3 ; group B: b1->b2 ; loner c
        let src = fns(&[
            ("a1", &["a2"]),
            ("a2", &["a3"]),
            ("a3", &[]),
            ("b1", &["b2"]),
            ("b2", &[]),
            ("c", &[]),
        ]);
        let exploded = explode(&src).unwrap();
        let plan = plan_split(&exploded, 10_000);
        assert_eq!(plan.parts.len(), 3, "two clusters + one loner");
        let sizes: Vec<usize> = plan.parts.iter().map(|p| p.chunk_indices.len()).collect();
        assert!(sizes.contains(&3) && sizes.contains(&2) && sizes.contains(&1));
    }

    #[test]
    fn a_cohesive_group_over_the_ceiling_is_split() {
        // a chain a->b->c->d->e, each ~4 LOC, ceiling 10 -> cannot be one module
        let src = fns(&[
            ("a", &["b"]),
            ("b", &["c"]),
            ("c", &["d"]),
            ("d", &["e"]),
            ("e", &[]),
        ]);
        let exploded = explode(&src).unwrap();
        let plan = plan_split(&exploded, 10);
        assert!(plan.parts.len() >= 2, "ceiling forces a split");
        for part in &plan.parts {
            assert!(part.loc < 10, "part over ceiling: {}", part.loc);
        }
    }

    #[test]
    fn imports_go_to_the_header_and_clusters_are_named() {
        let src = "use std::fmt;\n\nfn alpha() { beta(); }\n\nfn beta() {}\n";
        let exploded = explode(src).unwrap();
        let plan = plan_split(&exploded, 10_000);
        assert_eq!(plan.header.len(), 1);
        assert_eq!(plan.parts.len(), 1, "alpha+beta cluster");
        assert!(!plan.parts[0].name.is_empty());
        assert!(
            plan.parts[0]
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_')
        );
    }

    #[test]
    fn an_oversized_leaf_is_reported_not_forced() {
        let src =
            "fn big() {\n    let _a = 1;\n    let _b = 2;\n    let _c = 3;\n    let _d = 4;\n}\n";
        let exploded = explode(src).unwrap();
        let plan = plan_split(&exploded, 5);
        assert_eq!(plan.oversized.len(), 1);
        assert!(!plan.oversized[0].recoverable);
        assert!(plan.parts.is_empty());
    }

    // ----- razel regression: re-exports, mod decls, and body mods never cluster

    #[test]
    fn reexports_mod_decls_and_body_mods_are_kept_out_of_clustering() {
        let src = "use std::fmt;\n\nmod helpers;\n\npub use helpers::Helper;\n\n\
                   pub fn api() -> i32 {\n    1\n}\n\n\
                   #[cfg(test)]\nmod tests {\n    fn smoke() {}\n}\n";
        let exploded = explode(src).unwrap();
        let plan = plan_split(&exploded, 10_000);

        let text = |indices: &[usize]| -> String {
            indices
                .iter()
                .map(|&i| exploded.chunks[i].text.as_str())
                .collect()
        };
        let header = text(&plan.header);
        assert!(header.contains("use std::fmt;"), "plain import -> header");
        assert!(
            !header.contains("pub use"),
            "re-export is not a header import"
        );

        let root_items = text(&plan.root_items);
        assert!(
            root_items.contains("mod helpers;"),
            "content-less mod decl stays at the root"
        );
        assert!(
            root_items.contains("pub use helpers::Helper;"),
            "pub use re-export stays at the root"
        );

        assert_eq!(plan.mods.len(), 1, "body mod is extracted, never clustered");
        assert!(text(&plan.mods).contains("#[cfg(test)]"));
        assert_eq!(plan.parts.len(), 1, "only `api` is left to cluster");
        assert!(plan.oversized.is_empty());
    }

    // ----- razel regression: the ceiling is not a packing target ---------------

    #[test]
    fn a_big_connected_component_is_partitioned_not_packed_to_the_ceiling() {
        // A chain of 12 related fns, ~11 LOC each (~132 total), budget 100.
        // Packing-to-the-ceiling would emit one ~99-LOC file plus a remainder;
        // the balanced share is ceil(132 / ceil(132/50)) = 44 -> three parts.
        let mut src = String::new();
        for i in 0..12 {
            let call = if i < 11 {
                format!("    chain{}();\n", i + 1)
            } else {
                String::new()
            };
            src.push_str(&format!(
                "fn chain{i}() {{\n{call}    let _a = 1;\n    let _b = 2;\n    let _c = 3;\n    \
                 let _d = 4;\n    let _e = 5;\n    let _f = 6;\n    let _g = 7;\n}}\n\n"
            ));
        }
        let exploded = explode(&src).unwrap();
        let plan = plan_split(&exploded, 100);

        assert!(
            plan.parts.len() >= 3,
            "a ~132-LOC component under a 100 ceiling must give >= 3 parts, got {}: {:?}",
            plan.parts.len(),
            plan.parts.iter().map(|p| p.loc).collect::<Vec<_>>()
        );
        for part in &plan.parts {
            assert!(
                part.loc <= 50,
                "part must stay near the balanced share (<= half the ceiling), got {}",
                part.loc
            );
        }
    }

    #[test]
    fn satellites_of_a_full_hub_consolidate_instead_of_one_file_each() {
        // hub + 20 satellites that each call it: once the hub's cluster reaches
        // the balanced share, the leftover satellites are still transitively
        // related and must consolidate, not become 16 tiny files.
        let mut src = String::from("fn hub() {}\n\n");
        for i in 0..20 {
            src.push_str(&format!(
                "fn sat{i}() {{\n    hub();\n    let _x = {i};\n}}\n\n"
            ));
        }
        let exploded = explode(&src).unwrap();
        let plan = plan_split(&exploded, 60);

        assert!(
            plan.parts.len() <= 6,
            "satellites must consolidate, got {} parts",
            plan.parts.len()
        );
        for part in &plan.parts {
            assert!(part.loc <= 30, "part over the balanced share: {}", part.loc);
        }
    }
}
