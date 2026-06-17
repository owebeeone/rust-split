use std::collections::BTreeSet;
use std::fmt;
use std::ops::Range;

use quote::ToTokens;
use serde::{Deserialize, Serialize};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Parse(syn::Error),
    NonCharBoundary { offset: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(formatter, "parse error: {error}"),
            Self::NonCharBoundary { offset } => {
                write!(formatter, "span offset {offset} is not a UTF-8 boundary")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<syn::Error> for Error {
    fn from(error: syn::Error) -> Self {
        Self::Parse(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exploded {
    pub chunks: Vec<Chunk>,
    pub manifest: Manifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub byte_range: ByteRange,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

impl From<Range<usize>> for ByteRange {
    fn from(range: Range<usize>) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineSpan {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub rows: Vec<ManifestRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRow {
    pub idx: usize,
    pub chunk_index: usize,
    pub name: String,
    pub kind: String,
    pub byte_range: ByteRange,
    pub span: ByteRange,
    pub line_span: LineSpan,
    pub loc: usize,
    pub adjacency_hint: Vec<String>,
}

pub fn explode(src: &str) -> Result<Exploded> {
    let file = syn::parse_file(src)?;
    let mut items = Vec::with_capacity(file.items.len());
    for item in &file.items {
        let range = checked_range(src, item.span().byte_range())?;
        items.push(ItemInfo {
            range,
            name: item_name(item),
            kind: item_kind(item).to_owned(),
            adjacency_candidates: item_adjacency_candidates(item),
        });
    }

    if items.is_empty() {
        if src.is_empty() {
            return Ok(Exploded {
                chunks: Vec::new(),
                manifest: Manifest { rows: Vec::new() },
            });
        }
        return Ok(single_preamble(src));
    }

    let sibling_names = items
        .iter()
        .map(|item| item.name.clone())
        .collect::<BTreeSet<_>>();

    let mut chunk_ranges = Vec::with_capacity(items.len() + 1);
    let first_start = if has_inner_file_attr(&src[..items[0].range.start]) {
        if items[0].range.start > 0 {
            chunk_ranges.push(ChunkSeed {
                kind: ChunkSeedKind::Preamble,
                range: 0..items[0].range.start,
            });
        }
        items[0].range.start
    } else {
        0
    };

    for index in 0..items.len() {
        let start = if index == 0 {
            first_start
        } else {
            split_gap(src, items[index - 1].range.end, items[index].range.start)
        };
        let end = if index + 1 == items.len() {
            src.len()
        } else {
            split_gap(src, items[index].range.end, items[index + 1].range.start)
        };
        chunk_ranges.push(ChunkSeed {
            kind: ChunkSeedKind::Item(index),
            range: start..end,
        });
    }

    let mut chunks = Vec::with_capacity(chunk_ranges.len());
    let mut rows = Vec::with_capacity(chunk_ranges.len());
    for seed in chunk_ranges {
        let chunk_index = chunks.len();
        let text = src[seed.range.clone()].to_owned();
        chunks.push(Chunk {
            byte_range: seed.range.clone().into(),
            text,
        });

        match seed.kind {
            ChunkSeedKind::Preamble => rows.push(preamble_row(src, chunk_index, seed.range)),
            ChunkSeedKind::Item(item_index) => {
                let item = &items[item_index];
                let adjacency_hint = item
                    .adjacency_candidates
                    .iter()
                    .filter(|name| sibling_names.contains(*name) && *name != &item.name)
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                rows.push(ManifestRow {
                    idx: rows.len(),
                    chunk_index,
                    name: item.name.clone(),
                    kind: item.kind.clone(),
                    byte_range: seed.range.into(),
                    span: item.range.clone().into(),
                    line_span: line_span(src, &item.range),
                    loc: loc(&chunks[chunk_index].text),
                    adjacency_hint,
                });
            }
        }
    }

    Ok(Exploded {
        chunks,
        manifest: Manifest { rows },
    })
}

pub fn manifest_toml(manifest: &Manifest) -> Result<String> {
    toml::to_string_pretty(manifest).map_err(|error| {
        Error::Parse(syn::Error::new(
            proc_macro2::Span::call_site(),
            format!("manifest serialization failed: {error}"),
        ))
    })
}

#[derive(Debug)]
struct ItemInfo {
    range: Range<usize>,
    name: String,
    kind: String,
    adjacency_candidates: BTreeSet<String>,
}

#[derive(Debug)]
struct ChunkSeed {
    kind: ChunkSeedKind,
    range: Range<usize>,
}

#[derive(Debug)]
enum ChunkSeedKind {
    Preamble,
    Item(usize),
}

fn single_preamble(src: &str) -> Exploded {
    let chunk = Chunk {
        byte_range: ByteRange {
            start: 0,
            end: src.len(),
        },
        text: src.to_owned(),
    };
    Exploded {
        chunks: vec![chunk],
        manifest: Manifest {
            rows: vec![preamble_row(src, 0, 0..src.len())],
        },
    }
}

fn preamble_row(src: &str, chunk_index: usize, range: Range<usize>) -> ManifestRow {
    ManifestRow {
        idx: chunk_index,
        chunk_index,
        name: "__preamble".to_owned(),
        kind: "preamble".to_owned(),
        byte_range: range.clone().into(),
        span: range.clone().into(),
        line_span: line_span(src, &range),
        loc: loc(&src[range]),
        adjacency_hint: Vec::new(),
    }
}

fn checked_range(src: &str, range: Range<usize>) -> Result<Range<usize>> {
    if !src.is_char_boundary(range.start) {
        return Err(Error::NonCharBoundary {
            offset: range.start,
        });
    }
    if !src.is_char_boundary(range.end) {
        return Err(Error::NonCharBoundary { offset: range.end });
    }
    Ok(range)
}

fn has_inner_file_attr(gap: &str) -> bool {
    gap.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("//!") || line.starts_with("#![")
    })
}

fn split_gap(src: &str, start: usize, end: usize) -> usize {
    let gap = &src[start..end];
    if gap.trim().is_empty() {
        return end;
    }

    let mut offset = 0;
    let mut cut = 0;
    for line in gap.split_inclusive('\n') {
        offset += line.len();
        if line.trim().is_empty() {
            cut = offset;
        }
    }
    start + cut
}

fn line_span(src: &str, range: &Range<usize>) -> LineSpan {
    if src.is_empty() {
        return LineSpan { start: 1, end: 1 };
    }
    let start = byte_to_line(src, range.start);
    let end_offset = if range.end > range.start {
        range.end.saturating_sub(1)
    } else {
        range.end
    };
    let end = byte_to_line(src, end_offset);
    LineSpan { start, end }
}

fn byte_to_line(src: &str, offset: usize) -> usize {
    src[..offset.min(src.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn loc(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count().max(1)
    }
}

fn item_kind(item: &syn::Item) -> &'static str {
    match item {
        syn::Item::Const(_) => "const",
        syn::Item::Enum(_) => "enum",
        syn::Item::ExternCrate(_) => "extern_crate",
        syn::Item::Fn(_) => "fn",
        syn::Item::ForeignMod(_) => "foreign_mod",
        syn::Item::Impl(_) => "impl",
        syn::Item::Macro(item) if is_macro_rules(item) => "macro_rules",
        syn::Item::Macro(_) => "macro",
        syn::Item::Mod(_) => "mod",
        syn::Item::Static(_) => "static",
        syn::Item::Struct(_) => "struct",
        syn::Item::Trait(_) => "trait",
        syn::Item::TraitAlias(_) => "trait_alias",
        syn::Item::Type(_) => "type",
        syn::Item::Union(_) => "union",
        syn::Item::Use(_) => "use",
        syn::Item::Verbatim(_) => "verbatim",
        _ => "unknown",
    }
}

fn item_name(item: &syn::Item) -> String {
    match item {
        syn::Item::Const(item) => item.ident.to_string(),
        syn::Item::Enum(item) => item.ident.to_string(),
        syn::Item::ExternCrate(item) => item.ident.to_string(),
        syn::Item::Fn(item) => item.sig.ident.to_string(),
        syn::Item::ForeignMod(item) => {
            let name = item.abi.name.as_ref().map(|name| name.value());
            match name {
                Some(name) => format!("extern \"{name}\""),
                None => "extern".to_owned(),
            }
        }
        syn::Item::Impl(item) => impl_name(item),
        syn::Item::Macro(item) => macro_name(item),
        syn::Item::Mod(item) => item.ident.to_string(),
        syn::Item::Static(item) => item.ident.to_string(),
        syn::Item::Struct(item) => item.ident.to_string(),
        syn::Item::Trait(item) => item.ident.to_string(),
        syn::Item::TraitAlias(item) => item.ident.to_string(),
        syn::Item::Type(item) => item.ident.to_string(),
        syn::Item::Union(item) => item.ident.to_string(),
        syn::Item::Use(item) => use_tree_name(&item.tree),
        syn::Item::Verbatim(tokens) => tokens.to_string(),
        _ => "unknown".to_owned(),
    }
}

fn impl_name(item: &syn::ItemImpl) -> String {
    let self_ty = normalize_tokens(&item.self_ty.to_token_stream().to_string());
    if let Some((_, path, _)) = &item.trait_ {
        let trait_name = normalize_tokens(&path.to_token_stream().to_string());
        format!("impl {trait_name} for {self_ty}")
    } else {
        format!("impl {self_ty}")
    }
}

fn macro_name(item: &syn::ItemMacro) -> String {
    item.ident
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| normalize_tokens(&item.mac.path.to_token_stream().to_string()))
}

fn is_macro_rules(item: &syn::ItemMacro) -> bool {
    normalize_tokens(&item.mac.path.to_token_stream().to_string()) == "macro_rules"
}

fn use_tree_name(tree: &syn::UseTree) -> String {
    match tree {
        syn::UseTree::Path(path) => format!("{}::{}", path.ident, use_tree_name(&path.tree)),
        syn::UseTree::Name(name) => name.ident.to_string(),
        syn::UseTree::Rename(rename) => format!("{} as {}", rename.ident, rename.rename),
        syn::UseTree::Glob(_) => "*".to_owned(),
        syn::UseTree::Group(group) => {
            let names = group
                .items
                .iter()
                .map(use_tree_name)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{ {names} }}")
        }
    }
}

fn normalize_tokens(value: &str) -> String {
    value
        .replace(" :: ", "::")
        .replace(" < ", "<")
        .replace(" >", ">")
        .replace(" ,", ",")
        .replace("( ", "(")
        .replace(" )", ")")
        .replace("[ ", "[")
        .replace(" ]", "]")
}

fn item_adjacency_candidates(item: &syn::Item) -> BTreeSet<String> {
    let mut visitor = AdjacencyVisitor::default();
    visitor.visit_item(item);
    visitor.names
}

#[derive(Default)]
struct AdjacencyVisitor {
    names: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for AdjacencyVisitor {
    fn visit_ident(&mut self, ident: &'ast proc_macro2::Ident) {
        self.names.insert(ident.to_string());
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        for input in &item.sig.inputs {
            self.visit_fn_arg(input);
        }
        self.visit_block(&item.block);
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        self.visit_type(&item.ty);
        self.visit_expr(&item.expr);
    }

    fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
        self.visit_type(&item.ty);
        self.visit_expr(&item.expr);
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        self.visit_type(&item.ty);
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        self.visit_fields(&item.fields);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        for variant in &item.variants {
            self.visit_fields(&variant.fields);
            if let Some((_, discriminant)) = &variant.discriminant {
                self.visit_expr(discriminant);
            }
        }
    }

    fn visit_item_union(&mut self, item: &'ast syn::ItemUnion) {
        self.visit_fields_named(&item.fields);
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        for supertrait in &item.supertraits {
            self.visit_type_param_bound(supertrait);
        }
        for trait_item in &item.items {
            self.visit_trait_item(trait_item);
        }
    }

    fn visit_item_trait_alias(&mut self, item: &'ast syn::ItemTraitAlias) {
        for bound in &item.bounds {
            self.visit_type_param_bound(bound);
        }
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        self.visit_type(&item.self_ty);
        if let Some((_, path, _)) = &item.trait_ {
            self.visit_path(path);
        }
        for impl_item in &item.items {
            self.visit_impl_item(impl_item);
        }
    }

    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        self.visit_macro(&item.mac);
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if let Some((_, items)) = &item.content {
            for item in items {
                self.visit_item(item);
            }
        }
    }

    fn visit_pat_ident(&mut self, pat: &'ast syn::PatIdent) {
        self.names.insert(pat.ident.to_string());
        visit::visit_pat_ident(self, pat);
    }
}
