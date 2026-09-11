//! Toolchain resolution — the fixture god-file.
//!
//! Modeled on the real-world lib.rs that split mode was first rejected on: a
//! crate root holding an interconnected domain (vocabulary types, a registry,
//! the resolver, a resolution context), a `mod helpers;` file declaration, a
//! `pub use` re-export, and an under-budget `#[cfg(test)] mod tests`.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fmt;

mod helpers;

pub use helpers::HelperConfig;

/// An opaque constraint label (e.g. `"os:linux"`). A platform either carries a
/// constraint label or it doesn't — that is all matching needs here.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Constraint(pub String);

impl Constraint {
    /// Build a constraint from any label-ish string.
    pub fn new(label: &str) -> Self {
        Self(label.to_owned())
    }

    /// The `"<dimension>:"` prefix of the label, used to detect when two
    /// constraints disagree about the same dimension rather than merely
    /// differing.
    pub fn dimension(&self) -> &str {
        match self.0.split_once(':') {
            Some((dimension, _)) => dimension,
            None => self.0.as_str(),
        }
    }
}

impl fmt::Display for Constraint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A platform DEFINITION: the set of constraints it satisfies
/// (e.g. `[os:linux, cpu:x86_64]`).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Platform {
    pub constraints: Vec<Constraint>,
}

impl Platform {
    /// A platform satisfying the given constraint labels.
    pub fn with(labels: &[&str]) -> Self {
        Self {
            constraints: labels.iter().map(|label| Constraint::new(label)).collect(),
        }
    }

    /// Whether every wanted constraint is satisfied by this platform.
    pub fn satisfies(&self, wanted: &[Constraint]) -> bool {
        wanted
            .iter()
            .all(|constraint| self.constraints.contains(constraint))
    }

    /// The dimensions this platform pins (`os`, `cpu`, ...), sorted.
    pub fn dimensions(&self) -> BTreeSet<String> {
        self.constraints
            .iter()
            .map(|constraint| constraint.dimension().to_owned())
            .collect()
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let labels: Vec<String> = self
            .constraints
            .iter()
            .map(ToString::to_string)
            .collect();
        write!(formatter, "[{}]", labels.join(", "))
    }
}

/// A toolchain TYPE id (e.g. `"//tools/cpp:toolchain_type"`).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ToolchainType(pub String);

impl ToolchainType {
    /// Build a type id from a label string.
    pub fn new(label: &str) -> Self {
        Self(label.to_owned())
    }
}

impl fmt::Display for ToolchainType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// One requested toolchain type + its mandatory flag — the request's set
/// element.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ToolchainRequirement {
    pub toolchain_type: ToolchainType,
    pub mandatory: bool,
}

impl ToolchainRequirement {
    /// A mandatory requirement for the given type label.
    pub fn mandatory(label: &str) -> Self {
        Self {
            toolchain_type: ToolchainType::new(label),
            mandatory: true,
        }
    }

    /// An optional requirement for the given type label.
    pub fn optional(label: &str) -> Self {
        Self {
            toolchain_type: ToolchainType::new(label),
            mandatory: false,
        }
    }
}

/// A REGISTERED toolchain: the type it provides, the platform constraints its
/// target must satisfy, and those its execution host must satisfy.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RegisteredToolchain {
    pub toolchain_type: ToolchainType,
    pub label: String,
    pub target_compatible_with: Vec<Constraint>,
    pub exec_compatible_with: Vec<Constraint>,
}

impl RegisteredToolchain {
    /// Whether this toolchain can produce output for `target` while running
    /// on `exec`.
    pub fn compatible(&self, target: &Platform, exec: &Platform) -> bool {
        target.satisfies(&self.target_compatible_with)
            && exec.satisfies(&self.exec_compatible_with)
    }
}

/// A resolved (type -> toolchain label) selection for one execution platform.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ToolchainResolution {
    pub execution_platform: Platform,
    pub selected: BTreeMap<ToolchainType, String>,
}

impl ToolchainResolution {
    /// The selected toolchain label for a type, if any.
    pub fn selected_for(&self, toolchain_type: &ToolchainType) -> Option<&str> {
        self.selected
            .get(toolchain_type)
            .map(String::as_str)
    }
}

/// Why resolution failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ResolveError {
    /// A mandatory toolchain type had no compatible registered toolchain.
    NoMatchingToolchain(ToolchainType),
    /// No registered execution platform satisfied every mandatory type.
    NoViableExecutionPlatform,
    /// The request named the same type twice with conflicting flags.
    ConflictingRequirement(ToolchainType),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMatchingToolchain(toolchain_type) => {
                write!(formatter, "no toolchain for {toolchain_type}")
            }
            Self::NoViableExecutionPlatform => {
                write!(formatter, "no viable execution platform")
            }
            Self::ConflictingRequirement(toolchain_type) => {
                write!(formatter, "conflicting requirement for {toolchain_type}")
            }
        }
    }
}

/// The registry of toolchains and execution platforms visible to resolution.
///
/// Registration order is meaningful: earlier registrations win ties, matching
/// the "first registered wins" rule of the system this models.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    toolchains: Vec<RegisteredToolchain>,
    execution_platforms: Vec<Platform>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a toolchain (later registrations lose ties).
    pub fn register_toolchain(&mut self, toolchain: RegisteredToolchain) {
        self.toolchains.push(toolchain);
    }

    /// Register an execution platform (order is preference order).
    pub fn register_execution_platform(&mut self, platform: Platform) {
        self.execution_platforms.push(platform);
    }

    /// All registered toolchains providing the given type, in order.
    pub fn toolchains_for(&self, toolchain_type: &ToolchainType) -> Vec<&RegisteredToolchain> {
        self.toolchains
            .iter()
            .filter(|toolchain| &toolchain.toolchain_type == toolchain_type)
            .collect()
    }

    /// The registered execution platforms, in preference order.
    pub fn execution_platforms(&self) -> &[Platform] {
        &self.execution_platforms
    }

    /// The distinct toolchain types with at least one registration.
    pub fn known_types(&self) -> BTreeSet<ToolchainType> {
        self.toolchains
            .iter()
            .map(|toolchain| toolchain.toolchain_type.clone())
            .collect()
    }

    /// Total registration count, for diagnostics.
    pub fn len(&self) -> usize {
        self.toolchains.len()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.toolchains.is_empty()
    }
}

/// Merge duplicate requirements, rejecting conflicting flags for one type.
fn normalize_requirements(
    requirements: &[ToolchainRequirement],
) -> Result<Vec<ToolchainRequirement>, ResolveError> {
    let mut seen: BTreeMap<ToolchainType, bool> = BTreeMap::new();
    for requirement in requirements {
        match seen.get(&requirement.toolchain_type) {
            Some(&mandatory) if mandatory != requirement.mandatory => {
                return Err(ResolveError::ConflictingRequirement(
                    requirement.toolchain_type.clone(),
                ));
            }
            Some(_) => {}
            None => {
                seen.insert(requirement.toolchain_type.clone(), requirement.mandatory);
            }
        }
    }
    Ok(seen
        .into_iter()
        .map(|(toolchain_type, mandatory)| ToolchainRequirement {
            toolchain_type,
            mandatory,
        })
        .collect())
}

/// Select, for one execution platform, a toolchain per required type.
///
/// Returns `None` when a mandatory type has no compatible toolchain on this
/// execution platform (the caller then tries the next platform).
fn select_for_platform(
    registry: &Registry,
    requirements: &[ToolchainRequirement],
    target: &Platform,
    exec: &Platform,
    config: &HelperConfig,
) -> Option<BTreeMap<ToolchainType, String>> {
    let mut selected = BTreeMap::new();
    for requirement in requirements {
        let candidates = registry.toolchains_for(&requirement.toolchain_type);
        let bounded: Vec<&RegisteredToolchain> = match config.max_candidates {
            0 => candidates,
            bound => candidates.into_iter().take(bound).collect(),
        };
        let chosen = bounded
            .into_iter()
            .find(|toolchain| toolchain.compatible(target, exec));
        match chosen {
            Some(toolchain) => {
                selected.insert(
                    requirement.toolchain_type.clone(),
                    toolchain.label.clone(),
                );
            }
            None if requirement.mandatory => return None,
            None => {}
        }
    }
    Some(selected)
}

/// Resolve a toolchain context: pick the first registered execution platform
/// that satisfies every mandatory requirement, selecting one toolchain per
/// type.
///
/// This is the pure entry point dependents call as `<crate>::resolve`.
pub fn resolve(
    registry: &Registry,
    requirements: &[ToolchainRequirement],
    target: &Platform,
    config: &HelperConfig,
) -> Result<ToolchainResolution, ResolveError> {
    let requirements = normalize_requirements(requirements)?;
    for exec in registry.execution_platforms() {
        if let Some(selected) =
            select_for_platform(registry, &requirements, target, exec, config)
        {
            return Ok(ToolchainResolution {
                execution_platform: exec.clone(),
                selected,
            });
        }
    }
    if config.strict {
        return Err(ResolveError::NoViableExecutionPlatform);
    }
    // Permissive fallback: report the first mandatory type that cannot be
    // satisfied anywhere, which is the more actionable diagnostic.
    match first_unsatisfiable(registry, &requirements, target) {
        Some(toolchain_type) => Err(ResolveError::NoMatchingToolchain(toolchain_type)),
        None => Err(ResolveError::NoViableExecutionPlatform),
    }
}

/// The first mandatory requirement no registered toolchain can satisfy for
/// `target` on ANY execution platform.
fn first_unsatisfiable(
    registry: &Registry,
    requirements: &[ToolchainRequirement],
    target: &Platform,
) -> Option<ToolchainType> {
    requirements
        .iter()
        .filter(|requirement| requirement.mandatory)
        .find(|requirement| {
            let candidates = registry.toolchains_for(&requirement.toolchain_type);
            !candidates.iter().any(|toolchain| {
                registry
                    .execution_platforms()
                    .iter()
                    .any(|exec| toolchain.compatible(target, exec))
            })
        })
        .map(|requirement| requirement.toolchain_type.clone())
}

/// A memoizing wrapper around [`resolve`], keyed by requirement set — the
/// shape the node-function layer above this crate uses.
#[derive(Default)]
pub struct ResolutionContext {
    registry: Registry,
    config: HelperConfig,
    memo: HashMap<String, Result<ToolchainResolution, ResolveError>>,
}

impl ResolutionContext {
    /// A context over the given registry and config.
    pub fn new(registry: Registry, config: HelperConfig) -> Self {
        Self {
            registry,
            config,
            memo: HashMap::new(),
        }
    }

    /// Resolve with memoization; repeated identical requests hit the cache.
    pub fn resolve_cached(
        &mut self,
        requirements: &[ToolchainRequirement],
        target: &Platform,
    ) -> Result<ToolchainResolution, ResolveError> {
        let key = context_key(requirements, target);
        if let Some(hit) = self.memo.get(&key) {
            return hit.clone();
        }
        let outcome = resolve(&self.registry, requirements, target, &self.config);
        self.memo.insert(key, outcome.clone());
        outcome
    }

    /// Number of memoized outcomes, for tests and diagnostics.
    pub fn cached_len(&self) -> usize {
        self.memo.len()
    }

    /// Drop every memoized outcome (e.g. after mutating the registry).
    pub fn invalidate(&mut self) {
        self.memo.clear();
    }
}

/// A stable cache key for a (requirements, target) request.
fn context_key(requirements: &[ToolchainRequirement], target: &Platform) -> String {
    let mut normalized: Vec<String> = requirements
        .iter()
        .map(|requirement| {
            format!(
                "{}={}",
                requirement.toolchain_type,
                if requirement.mandatory { "!" } else { "?" }
            )
        })
        .collect();
    normalized.sort();
    format!("{}|{}", normalized.join(","), target)
}

/// Render a resolution as one diagnostic line per selected type.
pub fn render_resolution(resolution: &ToolchainResolution) -> Vec<String> {
    resolution
        .selected
        .iter()
        .map(|(toolchain_type, label)| {
            format!(
                "{toolchain_type} -> {label} on {}",
                resolution.execution_platform
            )
        })
        .collect()
}

/// Render a failed resolution for logs, prefixed by the requirement count.
pub fn render_failure(error: &ResolveError, requirements: &[ToolchainRequirement]) -> String {
    format!("{} requirement(s): {error}", requirements.len())
}

/// Summarize a registry: type -> registration count, for `--explain` output.
pub fn registry_summary(registry: &Registry) -> BTreeMap<ToolchainType, usize> {
    let mut summary: BTreeMap<ToolchainType, usize> = BTreeMap::new();
    for toolchain_type in registry.known_types() {
        let count = registry.toolchains_for(&toolchain_type).len();
        summary.insert(toolchain_type, count);
    }
    summary
}

/// Check a proposed execution platform against every mandatory requirement,
/// returning the types it cannot serve — the "why not this platform" probe.
pub fn unservable_types(
    registry: &Registry,
    requirements: &[ToolchainRequirement],
    target: &Platform,
    exec: &Platform,
) -> Vec<ToolchainType> {
    requirements
        .iter()
        .filter(|requirement| requirement.mandatory)
        .filter(|requirement| {
            !registry
                .toolchains_for(&requirement.toolchain_type)
                .iter()
                .any(|toolchain| toolchain.compatible(target, exec))
        })
        .map(|requirement| requirement.toolchain_type.clone())
        .collect()
}

/// How many constraints two platforms share — the affinity score used to rank
/// otherwise-viable execution platforms.
pub fn constraint_overlap(first: &Platform, second: &Platform) -> usize {
    first
        .constraints
        .iter()
        .filter(|constraint| second.constraints.contains(constraint))
        .count()
}

/// Rank the registry's execution platforms by affinity with the target,
/// preserving registration order among ties (stable sort).
pub fn rank_execution_platforms(registry: &Registry, target: &Platform) -> Vec<Platform> {
    let mut ranked: Vec<Platform> = registry.execution_platforms().to_vec();
    ranked.sort_by_key(|exec| std::cmp::Reverse(constraint_overlap(exec, target)));
    ranked
}

/// A human-readable record of the decisions one resolution made.
#[derive(Clone, Debug, Default)]
pub struct SelectionTrace {
    steps: Vec<String>,
}

impl SelectionTrace {
    /// Record one decision.
    pub fn note(&mut self, step: String) {
        self.steps.push(step);
    }

    /// The recorded decisions, oldest first.
    pub fn steps(&self) -> &[String] {
        &self.steps
    }

    /// Render the trace as an indented block for logs.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            out.push_str("  ");
            out.push_str(step);
            out.push('\n');
        }
        out
    }
}

/// [`resolve`], but ranking execution platforms by target affinity first and
/// recording each attempt into a trace.
pub fn traced_resolve(
    registry: &Registry,
    requirements: &[ToolchainRequirement],
    target: &Platform,
    config: &HelperConfig,
    trace: &mut SelectionTrace,
) -> Result<ToolchainResolution, ResolveError> {
    let requirements = normalize_requirements(requirements)?;
    for exec in rank_execution_platforms(registry, target) {
        trace.note(format!(
            "trying {exec} (overlap {})",
            constraint_overlap(&exec, target)
        ));
        if let Some(selected) =
            select_for_platform(registry, &requirements, target, &exec, config)
        {
            trace.note(format!("selected {} type(s)", selected.len()));
            return Ok(ToolchainResolution {
                execution_platform: exec,
                selected,
            });
        }
        let missing = unservable_types(registry, &requirements, target, &exec);
        trace.note(format!("rejected: {} unservable type(s)", missing.len()));
    }
    trace.note("no viable execution platform".to_owned());
    Err(ResolveError::NoViableExecutionPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cc(label: &str, os: &str) -> RegisteredToolchain {
        RegisteredToolchain {
            toolchain_type: ToolchainType::new("//cc:toolchain_type"),
            label: label.to_owned(),
            target_compatible_with: vec![Constraint::new(&format!("os:{os}"))],
            exec_compatible_with: vec![],
        }
    }

    fn linux_registry() -> Registry {
        let mut registry = Registry::new();
        registry.register_toolchain(cc("//cc:gcc_linux", "linux"));
        registry.register_toolchain(cc("//cc:clang_mac", "macos"));
        registry.register_execution_platform(Platform::with(&["os:linux"]));
        registry
    }

    #[test]
    fn resolves_first_compatible_toolchain() {
        let registry = linux_registry();
        let requirements = [ToolchainRequirement::mandatory("//cc:toolchain_type")];
        let target = Platform::with(&["os:linux"]);
        let resolution =
            resolve(&registry, &requirements, &target, &HelperConfig::permissive()).unwrap();
        assert_eq!(
            resolution.selected_for(&ToolchainType::new("//cc:toolchain_type")),
            Some("//cc:gcc_linux")
        );
    }

    #[test]
    fn memoizes_identical_requests() {
        let mut context =
            ResolutionContext::new(linux_registry(), HelperConfig::permissive());
        let requirements = [ToolchainRequirement::mandatory("//cc:toolchain_type")];
        let target = Platform::with(&["os:linux"]);
        let mut outcomes: HashMap<String, usize> = HashMap::new();
        for _ in 0..3 {
            let outcome = context.resolve_cached(&requirements, &target).unwrap();
            *outcomes
                .entry(outcome.selected.values().next().unwrap().clone())
                .or_insert(0) += 1;
        }
        assert_eq!(context.cached_len(), 1);
        assert_eq!(outcomes.len(), 1);
    }
}
