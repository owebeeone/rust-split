//! Tier 2 step 1 — the cohesion clusterer.
//!
//! This is **not** a bin-packer. The objective is to put **related items
//! together and unrelated items apart**. The number of output files is a
//! *consequence* of how related the code is, never something to minimise: if a
//! file genuinely contains ten unrelated 50-line items, ten 50-line files is the
//! correct answer, and merging any of them to "save a file" or "fill a bin"
//! makes the split worse. `max_loc` is only a **ceiling** — a cohesive group is
//! never merged past it; if a single group is already that big it is split
//! *further* along its weakest internal edges.
//!
//! Relation: an item references a sibling (the manifest's `adjacency_hint`).
//! Clustering is greedy agglomerative on those reference edges, heaviest edge
//! first, refusing any merge that would cross the ceiling. Items with no edges
//! stay alone. Imports/preamble are pulled out as a shared header (every module
//! over-includes it); an item that is itself `>= max_loc` is reported oversized.

use crate::Exploded;
use std::collections::{BTreeMap, HashMap};

/// The result of planning a split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPlan {
    /// Chunk indices forming the shared header (preamble + imports).
    pub header: Vec<usize>,
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
    let mut oversized = Vec::new();
    let mut items: Vec<Item> = Vec::new();

    for row in &exploded.manifest.rows {
        match row.kind.as_str() {
            "use" | "preamble" | "extern_crate" => header.push(row.chunk_index),
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

    if items.is_empty() {
        return SplitPlan {
            header,
            parts: Vec::new(),
            oversized,
        };
    }

    let parts = cluster(&items, max_loc);
    SplitPlan {
        header,
        parts,
        oversized,
    }
}

/// Greedy agglomerative clustering on reference edges, ceiling-capped.
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

    let mut parent: Vec<usize> = (0..n).collect();
    let mut group_loc: Vec<usize> = items.iter().map(|item| item.loc).collect();
    for ((i, j), _weight) in edges {
        let ri = find(&mut parent, i);
        let rj = find(&mut parent, j);
        if ri != rj && group_loc[ri] + group_loc[rj] < max_loc {
            parent[rj] = ri;
            group_loc[ri] += group_loc[rj];
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
}
