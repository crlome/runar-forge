//! Turning unresolved call sites into graph edges.
//!
//! There is deliberately no type inference here. A fixed chain of heuristics
//! runs in confidence order and the first one that matches wins; a call that
//! matches none of them is counted rather than attached to a plausible-looking
//! definition, and a tier that matches ambiguously falls through instead of
//! deciding by iteration order. An edge in this graph is therefore either
//! evidence or absent, and the gap is a number a consumer can see.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use crate::huginn::graph::parsers::Lang;

use super::store::EdgeRecord;
use super::{qualified_name, EdgeKind, FileExtract, RawSymbol, Resolution, SymbolLabel};

/// One file's extraction plus the path it came from.
pub struct FileFacts {
    pub path: String,
    pub extract: FileExtract,
}

#[derive(Debug, Default)]
pub struct Resolved {
    pub edges: Vec<EdgeRecord>,
    /// path -> number of call sites that matched no definition.
    pub unresolved: HashMap<String, usize>,
}

/// Resolve every call site and relation in `files` against every definition in
/// `files`.
pub fn resolve(files: &[FileFacts], project_root: &Path) -> Resolved {
    let index = Index::build(files, project_root);
    let mut out = Resolved::default();

    for from in files {
        for call in &from.extract.calls {
            // A qualifier names something — a module, a type, or the enclosing
            // one — and using it is far more precise than matching the callee
            // alone: `Store::open` and `File::open` are the same bare name.
            let resolved = match call.qualifier.as_deref() {
                // A receiver we could not place means the type is unknown. The
                // bare-name tiers assume there is no receiver, so applying them
                // here is what turns every `row.get(0)` into a call to whatever
                // single project symbol happens to be named `get`.
                Some(q) => index
                    .resolve_qualified(from, q, &call.callee, call.caller.as_deref())
                    .or_else(|| index.resolve_suffix(from, &call.callee)),
                None => index.resolve_unqualified(from, &call.callee),
            };
            let Some((target, tier)) = resolved else {
                *out.unresolved.entry(from.path.clone()).or_default() += 1;
                continue;
            };
            // A call at file scope has no definition to hang the edge on, so it
            // is resolved for counting purposes but cannot become an edge.
            let Some(source) = call.caller.as_deref() else {
                continue;
            };
            // A function calling itself adds nothing and would inflate the
            // fan-in that hub ranking reads.
            if source == target {
                continue;
            }
            out.edges.push(EdgeRecord {
                source_qualified: source.to_string(),
                target_qualified: target,
                kind: EdgeKind::Calls,
                resolution: Some(tier),
                line: call.line,
            });
        }

        for rel in &from.extract.relations {
            // Both ends are type names, so the suffix tier — which exists for
            // method calls on a receiver — does not apply.
            let (Some((source, _)), Some((target, _))) = (
                index.resolve_type_name(from, &rel.subject),
                index.resolve_type_name(from, &rel.object),
            ) else {
                continue;
            };
            if source == target {
                continue;
            }
            out.edges.push(EdgeRecord {
                source_qualified: source,
                target_qualified: target,
                kind: rel.kind,
                // Syntactic, not inferred: there is no tier to record.
                resolution: None,
                line: 0,
            });
        }
    }

    out
}

/// Everything an import's resolution actually depends on: the importing file's
/// directory, whether that file *is* its directory's module (which is what
/// `super` hinges on), and the path as written. Files in one directory import
/// the same modules over and over, and each miss costs a handful of `stat`s.
type ImportKey = (PathBuf, bool, String);

/// Every lookup the tier chain needs, built once for the whole project.
struct Index<'a> {
    defs_by_name: HashMap<&'a str, Vec<(&'a FileFacts, &'a RawSymbol)>>,
    defs_by_file: HashMap<&'a str, Vec<&'a RawSymbol>>,
    /// file path -> (locally bound name -> file that binding resolves to).
    imports: HashMap<&'a str, HashMap<&'a str, String>>,
    /// Names of types that can own methods, for the suffix tier.
    containers: HashSet<&'a str>,
    project_root: PathBuf,
}

/// The container out of a qualified name: `src/a.rs:Engine.run` → `Engine`.
/// A file path may contain dots, so the split is anchored on the last colon.
fn container_of_qualified(qualified: &str) -> Option<&str> {
    let tail = qualified.rsplit(':').next()?;
    let (container, _) = tail.rsplit_once('.')?;
    (!container.is_empty()).then_some(container)
}

impl<'a> Index<'a> {
    fn build(files: &'a [FileFacts], project_root: &Path) -> Self {
        let mut defs_by_name: HashMap<&str, Vec<(&FileFacts, &RawSymbol)>> = HashMap::new();
        let mut defs_by_file: HashMap<&str, Vec<&RawSymbol>> = HashMap::new();
        let mut containers: HashSet<&str> = HashSet::new();
        let mut imports: HashMap<&str, HashMap<&str, String>> = HashMap::new();
        let mut seen: HashMap<ImportKey, Option<String>> = HashMap::new();

        for file in files {
            for sym in &file.extract.symbols {
                defs_by_name
                    .entry(sym.name.as_str())
                    .or_default()
                    .push((file, sym));
                defs_by_file
                    .entry(file.path.as_str())
                    .or_default()
                    .push(sym);
                if sym.label.is_container() {
                    containers.insert(sym.name.as_str());
                }
            }

            let here = Path::new(file.path.as_str());
            let dir = here.parent().unwrap_or(here).to_path_buf();
            let module_root = is_module_root_file(here);

            let mut bound: HashMap<&str, String> = HashMap::new();
            for imp in &file.extract.imports {
                let key = (dir.clone(), module_root, imp.source.clone());
                let target = seen
                    .entry(key)
                    .or_insert_with(|| resolve_import(&imp.source, &file.path, project_root));
                if let Some(target) = target {
                    bound.insert(imp.local_name.as_str(), target.clone());
                }
            }
            imports.insert(file.path.as_str(), bound);
        }

        Self {
            project_root: project_root.to_path_buf(),
            defs_by_name,
            defs_by_file,
            imports,
            containers,
        }
    }

    /// Resolve a call written with a receiver, using what the receiver names.
    ///
    /// Three shapes carry real information: the enclosing type (`self`/`this`),
    /// a module path (`crate::util::spin`), and a type name (`Store::open`).
    /// Anything else — a local variable — needs types this layer does not have,
    /// and falls through to the name-only chain.
    fn resolve_qualified(
        &self,
        from: &FileFacts,
        qualifier: &str,
        callee: &str,
        caller: Option<&str>,
    ) -> Option<(String, Resolution)> {
        if matches!(qualifier, "self" | "Self" | "this") {
            let owner = caller.and_then(container_of_qualified)?;
            return self.member_of(Some(&from.path), owner, callee, Resolution::SameModule);
        }

        if qualifier.contains("::") || qualifier.starts_with('.') {
            if let Some(target) = resolve_import(qualifier, &from.path, &self.project_root) {
                if let Some(sym) = self
                    .defs_by_file
                    .get(target.as_str())
                    .and_then(|defs| only(defs.iter().copied().filter(|s| s.name == callee)))
                {
                    return Some((
                        qualified_name(&target, sym.container.as_deref(), &sym.name),
                        Resolution::ImportMap,
                    ));
                }
            }
        }

        // A single-segment module name — `index::index_project(..)`. The callee
        // is a free function in that module's file, not a member of anything,
        // so the type branch below can never find it.
        for target in self.module_candidates(from, qualifier) {
            if let Some(sym) = self.defs_by_file.get(target.as_str()).and_then(|defs| {
                only(
                    defs.iter()
                        .copied()
                        .filter(|s| s.name == callee && s.container.is_none() && is_callable(s)),
                )
            }) {
                return Some((
                    qualified_name(&target, None, &sym.name),
                    Resolution::ImportMap,
                ));
            }
        }

        // A bare type name: find the type, then the member on it. The tier is
        // how confidently the *type* was located.
        let (tier, owner_file) = self.locate_type(from, qualifier)?;
        self.member_of(Some(&owner_file), qualifier, callee, tier)
    }

    /// The file a module name refers to from `from`: either where an import
    /// bound it, or a sibling `name.rs` / `name/mod.rs` declared with `mod`.
    fn module_candidates(&self, from: &FileFacts, name: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let bound = self
            .imports
            .get(from.path.as_str())
            .and_then(|m| m.get(name))
            .cloned();

        // An import of `crate::codegraph::{index, ..}` binds `index` to the
        // path that resolved — `codegraph/mod.rs` — so the submodule beside it
        // comes first, then the bound file itself.
        if let Some(bound) = &bound {
            let dir = Path::new(bound.as_str()).parent().unwrap_or(Path::new(""));
            out.push(join_rel(dir, &format!("{name}.rs")));
            out.push(join_rel(dir, &format!("{name}/mod.rs")));
            out.push(bound.clone());
        }

        // A sibling file only counts when this file actually declares the
        // module. Without that check any receiver variable sharing a name with
        // a neighbouring file would be read as a module path.
        if self.declares_module(from, name) {
            let here = Path::new(from.path.as_str())
                .parent()
                .unwrap_or(Path::new(""));
            out.push(join_rel(here, &format!("{name}.rs")));
            out.push(join_rel(here, &format!("{name}/mod.rs")));
        }

        out.retain(|c| self.defs_by_file.contains_key(c.as_str()));
        out
    }

    /// Whether `from` contains `mod <name>;`, which the extractor records as a
    /// Module symbol.
    fn declares_module(&self, from: &FileFacts, name: &str) -> bool {
        self.defs_by_file
            .get(from.path.as_str())
            .is_some_and(|defs| {
                defs.iter()
                    .any(|s| s.name == name && s.label == SymbolLabel::Module)
            })
    }

    /// The single definition named `callee` owned by container `owner`.
    ///
    /// Prefers the file the owner was located in. Rust lets an `impl` block sit
    /// in a different file from the type it extends, so a miss there falls back
    /// to the whole project — but only when unambiguous, and the tier drops to
    /// reflect that the file was a guess rather than the one we placed.
    fn member_of(
        &self,
        prefer_file: Option<&str>,
        owner: &str,
        callee: &str,
        tier: Resolution,
    ) -> Option<(String, Resolution)> {
        let candidates = self.defs_by_name.get(callee)?;
        let owned = |s: &RawSymbol| is_callable(s) && s.container.as_deref() == Some(owner);

        if let Some(file) = prefer_file {
            if let Some(sym) = only(
                self.defs_by_file
                    .get(file)?
                    .iter()
                    .copied()
                    .filter(|s| s.name == callee && owned(s)),
            ) {
                return Some((
                    qualified_name(file, sym.container.as_deref(), &sym.name),
                    tier,
                ));
            }
        }

        let (file, sym) = only(candidates.iter().copied().filter(|(_, s)| owned(s)))?;
        let _ = tier;
        Some((
            qualified_name(&file.path, sym.container.as_deref(), &sym.name),
            Resolution::UniqueName,
        ))
    }

    /// Where a bare type name resolves to, and how confidently.
    fn locate_type(&self, from: &FileFacts, name: &str) -> Option<(Resolution, String)> {
        let defined_in = |path: &str| {
            self.defs_by_file
                .get(path)
                .is_some_and(|defs| defs.iter().any(|s| s.name == name))
        };
        if let Some(target) = self
            .imports
            .get(from.path.as_str())
            .and_then(|m| m.get(name))
            .filter(|target| defined_in(target.as_str()))
        {
            return Some((Resolution::ImportMap, target.clone()));
        }
        if defined_in(from.path.as_str()) {
            return Some((Resolution::SameModule, from.path.clone()));
        }
        let (file, _) = only(self.defs_by_name.get(name)?.iter().copied())?;
        Some((Resolution::UniqueName, file.path.clone()))
    }

    /// A call written without a receiver. It can only reach something callable
    /// that needs no receiver, which rules out methods — and modules, which are
    /// not callable at all yet share their name with `Vec::push` and friends.
    fn resolve_unqualified(&self, from: &FileFacts, name: &str) -> Option<(String, Resolution)> {
        self.resolve_name(from, name, |s| is_callable(s) && s.container.is_none())
    }

    /// A type name, for inheritance and implementation relations.
    fn resolve_type_name(&self, from: &FileFacts, name: &str) -> Option<(String, Resolution)> {
        self.resolve_name(from, name, |s| s.label != SymbolLabel::Module)
    }

    /// Last resort for a method call on a receiver of unknown type: accept it
    /// only when exactly one project type defines a method by that name.
    fn resolve_suffix(&self, from: &FileFacts, name: &str) -> Option<(String, Resolution)> {
        if is_ubiquitous_method(name) {
            return None;
        }
        // The owning type must be one this file can actually name — imported
        // here or defined here. Without that the tier reasons only "some
        // project type has a method by this name", which is true of a great
        // many names the receiver never had.
        let visible = |owner: &str| {
            self.imports
                .get(from.path.as_str())
                .is_some_and(|m| m.contains_key(owner))
                || self
                    .defs_by_file
                    .get(from.path.as_str())
                    .is_some_and(|defs| defs.iter().any(|s| s.name == owner))
        };
        let (file, sym) = only(
            self.defs_by_name
                .get(name)?
                .iter()
                .copied()
                .filter(|(_, s)| {
                    is_callable(s)
                        && s.container
                            .as_deref()
                            .is_some_and(|owner| self.containers.contains(owner) && visible(owner))
                }),
        )?;
        Some((
            qualified_name(&file.path, sym.container.as_deref(), &sym.name),
            Resolution::Suffix,
        ))
    }

    /// The tier chain, first match wins, over the symbols `keep` admits.
    fn resolve_name(
        &self,
        from: &FileFacts,
        name: &str,
        keep: impl Fn(&RawSymbol) -> bool + Copy,
    ) -> Option<(String, Resolution)> {
        if let Some(target) = self
            .imports
            .get(from.path.as_str())
            .and_then(|m| m.get(name))
        {
            if let Some(sym) = self
                .defs_by_file
                .get(target.as_str())
                .and_then(|defs| only(defs.iter().copied().filter(|s| s.name == name && keep(s))))
            {
                return Some((
                    qualified_name(target, sym.container.as_deref(), &sym.name),
                    Resolution::ImportMap,
                ));
            }
        }

        if let Some(sym) = self
            .defs_by_file
            .get(from.path.as_str())
            .and_then(|defs| only(defs.iter().copied().filter(|s| s.name == name && keep(s))))
        {
            return Some((
                qualified_name(&from.path, sym.container.as_deref(), &sym.name),
                Resolution::SameModule,
            ));
        }

        let by_name = self.defs_by_name.get(name);

        if let Some((file, sym)) =
            by_name.and_then(|c| only(c.iter().copied().filter(|(_, s)| keep(s))))
        {
            return Some((
                qualified_name(&file.path, sym.container.as_deref(), &sym.name),
                Resolution::UniqueName,
            ));
        }

        None
    }
}

/// Modules cannot be called and constants are not call targets, yet both share
/// their names with extremely common methods (`push`, `write`, `len`), so
/// leaving them in the candidate set makes the weak tiers manufacture edges.
///
/// Fields are the same case and a worse one: `self.handler`, `s.write`,
/// `c.next` are ordinary field names, and a struct has far more fields than
/// methods, so admitting them would give the unique-name and suffix tiers a
/// large pool of plausible wrong answers. A call through a field reaches
/// whatever the field holds, which the graph does not know.
fn is_callable(sym: &RawSymbol) -> bool {
    !matches!(
        sym.label,
        SymbolLabel::Module | SymbolLabel::Const | SymbolLabel::Field
    )
}

/// Methods that the standard library puts on nearly every type.
///
/// The suffix tier reasons "only one project type defines this method, so the
/// call must mean that one" — sound for a distinctive name, and wrong for these,
/// because the receiver is overwhelmingly a `String`, `Vec` or `Result` the
/// graph cannot see. Left in, a single `ChangedFiles::is_empty` collects an
/// edge from every `.is_empty()` in the project.
fn is_ubiquitous_method(name: &str) -> bool {
    COMMON_METHOD_NAMES.binary_search(&name).is_ok()
}

/// Sorted; `is_ubiquitous_method` binary-searches it.
const COMMON_METHOD_NAMES: &[&str] = &[
    "as_ref",
    "as_str",
    "clone",
    "cloned",
    "collect",
    "contains",
    "count",
    "err",
    "extend",
    "filter",
    "find",
    "first",
    "get",
    "get_mut",
    "insert",
    "is_empty",
    "is_err",
    "is_none",
    "is_ok",
    "is_some",
    "iter",
    "join",
    "keys",
    "last",
    "len",
    "map",
    "next",
    "ok",
    "ok_or",
    "parse",
    "pop",
    "push",
    "push_str",
    "read",
    "remove",
    "replace",
    "reverse",
    "send",
    "set",
    "sort",
    "split",
    "take",
    "then",
    "to_owned",
    "to_string",
    "trim",
    "unwrap",
    "unwrap_or",
    "values",
    "write",
];

/// The single element of an iterator, or None if there are none or several.
/// Ambiguity has to fall through to the next tier rather than be settled by
/// iteration order, which is not stable across runs.
fn only<T>(mut items: impl Iterator<Item = T>) -> Option<T> {
    let first = items.next()?;
    items.next().is_none().then_some(first)
}

fn lang_of(path: &str) -> Option<Lang> {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(Lang::from_extension)
}

/// One import's module path resolved to a project-relative file path, or None
/// when the path leaves the project.
fn resolve_import(source: &str, from: &str, project_root: &Path) -> Option<String> {
    match lang_of(from)? {
        Lang::Rust => resolve_rust_module(source, Path::new(from), project_root),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            resolve_relative_module(source, Path::new(from), project_root)
        }
        Lang::Python | Lang::Go => None,
    }
}

/// `crate::a::b::C`, `super::x` and `self::x` resolved to the file that holds
/// the named item. `from` is project-relative, as is the result.
fn resolve_rust_module(source: &str, from: &Path, project_root: &Path) -> Option<String> {
    let from_abs = project_root.join(from);
    let from_dir = from_abs.parent()?;
    let segments: Vec<&str> = source
        .split("::")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let first = *segments.first()?;

    // Whether the path is known to be first-party, which is what licenses the
    // fallback to the module's own file below.
    let mut self_named = false;
    let (base, rest) = match first {
        "crate" => (crate_src_root(&from_abs, project_root)?, &segments[1..]),
        // The file's own directory, not `<stem>/`: a name written `self::x` is
        // far more often a sibling module than a child module declared inside a
        // non-`mod.rs` file.
        "self" => (from_dir.to_path_buf(), &segments[1..]),
        "super" => {
            let levels = segments.iter().take_while(|s| **s == "super").count();
            let mut dir = if is_module_root_file(&from_abs) {
                from_dir.parent()?.to_path_buf()
            } else {
                from_dir.to_path_buf()
            };
            for _ in 1..levels {
                dir = dir.parent()?.to_path_buf();
            }
            (dir, &segments[levels..])
        }
        // A crate naming *itself*. A binary, an example or an integration test
        // reaches its own library through the package name rather than
        // `crate::` — `use runar_muninn::hooks_runtime` — and without this the
        // entire library looks external from those files, so every call they
        // make into it resolves to nothing. That is not an edge case here:
        // `main.rs` is the largest file in this project.
        _ if crate_lib_name(&from_abs, project_root).as_deref() == Some(first) => {
            self_named = true;
            (crate_src_root(&from_abs, project_root)?, &segments[1..])
        }
        // Either a 2015-edition path rooted at the crate, or another crate.
        // Only the first can name a file here, and an unmatched one stays
        // unresolved rather than being pinned to a same-named local module.
        _ => (
            crate_src_root(&from_abs, project_root)?,
            segments.as_slice(),
        ),
    };

    for candidate in module_candidates(&base, rest) {
        if candidate.is_file() {
            return relativize(&candidate, project_root);
        }
    }

    // Only a path known to be first-party may fall back to the module's own
    // file (an item declared in an inline `mod`, or in `lib.rs`/`mod.rs`
    // directly). For `use <own_crate>::{a, b}` that fallback is the whole
    // point: the source is the crate name alone, so there are no segments left
    // to name a file with, and the answer is the crate root — from which the
    // caller's `module_candidates` finds `a.rs` beside it.
    if self_named || matches!(first, "crate" | "super" | "self") {
        for candidate in module_file_candidates(&base) {
            if candidate.is_file() {
                return relativize(&candidate, project_root);
            }
        }
    }

    None
}

/// `[a, b, C]` under `src/` yields `src/a/b/C.rs`, `src/a/b/C/mod.rs`,
/// `src/a/b.rs`, `src/a/b/mod.rs`, `src/a.rs`, `src/a/mod.rs`.
///
/// Trailing segments are stripped because they name items rather than modules.
/// Nothing is case-folded: a module segment is already spelled the way its file
/// is, so folding a CamelCase item segment down could only invent a match.
fn module_candidates(base: &Path, segments: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(segments.len() * 2);
    for take in (1..=segments.len()).rev() {
        let mut dir = base.to_path_buf();
        for seg in &segments[..take - 1] {
            dir.push(seg);
        }
        let last = segments[take - 1];
        out.push(dir.join(format!("{last}.rs")));
        out.push(dir.join(last).join("mod.rs"));
    }
    out
}

/// The files that can define the module a directory stands for, in both the
/// 2015 (`d/mod.rs`) and 2018 (`d.rs`) layouts, plus the crate roots for `src/`.
fn module_file_candidates(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![dir.join("mod.rs")];
    if let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) {
        let mut sibling = name.to_os_string();
        sibling.push(".rs");
        out.push(parent.join(sibling));
    }
    out.push(dir.join("lib.rs"));
    out.push(dir.join("main.rs"));
    out
}

/// `mod.rs`, `lib.rs` and `main.rs` *are* their directory's module, so `super`
/// from one of them means the directory's parent, not the directory.
fn is_module_root_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("mod.rs" | "lib.rs" | "main.rs")
    )
}

/// The `src/` of the nearest enclosing Cargo package, which is what `crate::`
/// is rooted at in a workspace. The walk stops at the project root so a
/// `Cargo.toml` above the crawl can never pull paths outside it.
/// The library name of the crate owning `from_abs`, as another file inside the
/// same crate would spell it in a `use`.
///
/// Cargo derives it from `[lib] name` when present and otherwise from the
/// package name with dashes turned into underscores, so `runar-muninn` is
/// imported as `runar_muninn`. Read rather than guessed: inferring "an unknown
/// first segment might be this crate" would bind `tokio::fs` to a project
/// `src/fs.rs`.
///
/// Memoized on the manifest directory — this is consulted once per distinct
/// import path per directory, and a workspace crawl has many of both.
fn crate_lib_name(from_abs: &Path, project_root: &Path) -> Option<String> {
    use std::sync::{LazyLock, Mutex};
    static CACHE: LazyLock<Mutex<HashMap<PathBuf, Option<String>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    let manifest_dir = {
        let mut dir = from_abs.parent();
        loop {
            let d = dir?;
            if d.join("Cargo.toml").is_file() {
                break d.to_path_buf();
            }
            if d == project_root {
                return None;
            }
            dir = d.parent();
        }
    };

    if let Ok(cache) = CACHE.lock() {
        if let Some(hit) = cache.get(&manifest_dir) {
            return hit.clone();
        }
    }

    let name = std::fs::read_to_string(manifest_dir.join("Cargo.toml"))
        .ok()
        .and_then(|toml| lib_name_from_manifest(&toml));
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(manifest_dir, name.clone());
    }
    name
}

/// `[lib] name` wins over `[package] name`; dashes become underscores.
///
/// Parsed by hand rather than with a TOML crate: only two keys matter, and the
/// section a key sits in is the whole of the logic.
fn lib_name_from_manifest(toml: &str) -> Option<String> {
    let mut section = "";
    let mut package: Option<String> = None;
    let mut lib: Option<String> = None;
    for line in toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line.trim_matches(['[', ']']);
            continue;
        }
        let Some(rest) = line.strip_prefix("name") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if value.is_empty() {
            continue;
        }
        match section {
            "package" => package = Some(value.to_string()),
            "lib" => lib = Some(value.to_string()),
            _ => {}
        }
    }
    lib.or(package).map(|n| n.replace('-', "_"))
}

fn crate_src_root(from_abs: &Path, project_root: &Path) -> Option<PathBuf> {
    let mut dir = from_abs.parent();
    while let Some(d) = dir {
        if d.join("Cargo.toml").is_file() {
            return Some(d.join("src"));
        }
        if d == project_root {
            return None;
        }
        dir = d.parent();
    }
    None
}

/// `./m` and `../m` next to the importing file. A bare specifier names a
/// package, which is outside the project by definition.
fn resolve_relative_module(source: &str, from: &Path, project_root: &Path) -> Option<String> {
    if !(source.starts_with("./") || source.starts_with("../")) {
        return None;
    }
    let base = normalize(&project_root.join(from).parent()?.join(source));

    if base.is_file() {
        return relativize(&base, project_root);
    }
    for ext in [".ts", ".tsx", ".js", ".jsx"] {
        let mut named = base.clone().into_os_string();
        named.push(ext);
        let candidate = PathBuf::from(named);
        if candidate.is_file() {
            return relativize(&candidate, project_root);
        }
    }
    for index in ["index.ts", "index.js"] {
        let candidate = base.join(index);
        if candidate.is_file() {
            return relativize(&candidate, project_root);
        }
    }
    None
}

/// Project-relative, stringified once at the end so the separator convention
/// matches the scanner's — these strings are compared against `FileFacts::path`.
fn relativize(path: &Path, project_root: &Path) -> Option<String> {
    let rel = normalize(path);
    let root = normalize(project_root);
    let stripped = rel.strip_prefix(root.as_path()).ok()?;
    Some(stripped.to_string_lossy().into_owned())
}

/// Lexical normalisation only: `..` is resolved against the path text and never
/// against the filesystem, so this neither touches disk nor follows symlinks.
/// Join a relative directory to a relative path, in the separator convention
/// the scanner produced — these strings are compared against `FileFacts::path`.
fn join_rel(dir: &Path, tail: &str) -> String {
    let joined = if dir.as_os_str().is_empty() {
        PathBuf::from(tail)
    } else {
        dir.join(tail)
    };
    joined.to_string_lossy().to_string()
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.file_name().is_some() {
                    out.pop();
                } else {
                    out.push(Component::ParentDir);
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegraph::{RawCall, RawImport, RawRelation, SymbolLabel, SymbolMetrics};

    fn sym(name: &str, label: SymbolLabel, container: Option<&str>) -> RawSymbol {
        RawSymbol {
            name: name.to_string(),
            label,
            container: container.map(str::to_string),
            start_line: 1,
            end_line: 20,
            signature: String::new(),
            exported: true,
            metrics: SymbolMetrics::default(),
        }
    }

    fn func(name: &str) -> RawSymbol {
        sym(name, SymbolLabel::Function, None)
    }

    fn call(caller: &str, callee: &str, qualifier: Option<&str>) -> RawCall {
        RawCall {
            caller: Some(caller.to_string()),
            callee: callee.to_string(),
            qualifier: qualifier.map(str::to_string),
            line: 7,
        }
    }

    fn facts(path: &str, symbols: Vec<RawSymbol>, calls: Vec<RawCall>) -> FileFacts {
        FileFacts {
            path: path.to_string(),
            extract: FileExtract {
                symbols,
                calls,
                ..Default::default()
            },
        }
    }

    /// Expected paths have to be spelled the way the scanner spells them.
    fn native(path: &str) -> String {
        let mut out = PathBuf::new();
        for seg in path.split('/') {
            out.push(seg);
        }
        out.to_string_lossy().into_owned()
    }

    fn touch(root: &Path, rel: &str) {
        let path = root.join(native(rel));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
    }

    fn nowhere() -> &'static Path {
        Path::new("/does/not/exist")
    }

    // ---- tier chain -------------------------------------------------------

    #[test]
    fn same_module_wins_over_a_definition_elsewhere() {
        let files = vec![
            facts(
                "a.rs",
                vec![func("caller"), func("helper")],
                vec![call("a.rs:caller", "helper", None)],
            ),
            facts("b.rs", vec![func("helper")], vec![]),
        ];
        let out = resolve(&files, nowhere());
        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].target_qualified, "a.rs:helper");
        assert_eq!(out.edges[0].resolution, Some(Resolution::SameModule));
        assert_eq!(out.edges[0].source_qualified, "a.rs:caller");
        assert_eq!(out.edges[0].kind, EdgeKind::Calls);
        assert_eq!(out.edges[0].line, 7);
        assert!(out.unresolved.is_empty());
    }

    #[test]
    fn a_lone_definition_project_wide_resolves_by_name() {
        let files = vec![
            facts(
                "a.rs",
                vec![func("caller")],
                vec![call("a.rs:caller", "helper", None)],
            ),
            facts("b.rs", vec![func("helper")], vec![]),
        ];
        let out = resolve(&files, nowhere());
        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].target_qualified, "b.rs:helper");
        assert_eq!(out.edges[0].resolution, Some(Resolution::UniqueName));
    }

    #[test]
    fn two_definitions_of_a_name_resolve_to_neither() {
        let files = vec![
            facts(
                "a.rs",
                vec![func("caller")],
                vec![call("a.rs:caller", "helper", None)],
            ),
            facts("b.rs", vec![func("helper")], vec![]),
            facts("c.rs", vec![func("helper")], vec![]),
        ];
        let out = resolve(&files, nowhere());
        assert!(
            out.edges.is_empty(),
            "an ambiguous name must not be guessed"
        );
        assert_eq!(out.unresolved.get("a.rs"), Some(&1));
    }

    #[test]
    fn a_qualified_call_falls_through_ambiguity_to_the_suffix_tier() {
        // `Widget` is defined in the calling file, so it is a type this call
        // site could actually be holding.
        let files = vec![
            facts(
                "a.rs",
                vec![
                    func("caller"),
                    sym("Widget", SymbolLabel::Struct, None),
                    sym("helper", SymbolLabel::Method, Some("Widget")),
                ],
                vec![call("a.rs:caller", "helper", Some("thing"))],
            ),
            facts("c.rs", vec![func("helper")], vec![]),
        ];
        let out = resolve(&files, nowhere());
        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].target_qualified, "a.rs:Widget.helper");
        assert_eq!(out.edges[0].resolution, Some(Resolution::Suffix));
    }

    #[test]
    fn a_module_qualified_call_reaches_a_free_function() {
        // `index::index_project(..)` — the callee belongs to no type, so the
        // type branch cannot find it and the bare-name tiers do not apply to a
        // qualified call.
        let a = native("src/a.rs");
        let idx = native("src/index.rs");
        let files = vec![
            facts(
                &a,
                // `mod index;` is what makes `index::` a module path here.
                vec![func("caller"), sym("index", SymbolLabel::Module, None)],
                vec![call(&format!("{a}:caller"), "run_index", Some("index"))],
            ),
            facts(&idx, vec![func("run_index")], vec![]),
        ];
        let out = resolve(&files, nowhere());
        assert_eq!(out.edges.len(), 1, "got {:?}", out.edges);
        assert_eq!(out.edges[0].target_qualified, format!("{idx}:run_index"));
        assert_eq!(out.edges[0].resolution, Some(Resolution::ImportMap));
    }

    /// Fields share names with methods constantly (`write`, `next`, `handler`)
    /// and a project has far more of them than methods, so admitting them to
    /// the candidate set hands the weak tiers a large pool of wrong answers.
    /// A call through a field reaches whatever the field holds, which the
    /// graph cannot see, so the honest result is no edge.
    ///
    /// The Method half is the control: it proves the shape resolves at all, so
    /// the Field half is failing for the intended reason and not because the
    /// fixture never reached the suffix tier.
    #[test]
    fn a_field_is_never_a_call_target() {
        let build = |label: SymbolLabel| {
            vec![facts(
                "a.rs",
                vec![
                    func("caller"),
                    sym("Widget", SymbolLabel::Struct, None),
                    sym("handler", label, Some("Widget")),
                ],
                vec![call("a.rs:caller", "handler", Some("w"))],
            )]
        };

        let control = resolve(&build(SymbolLabel::Method), nowhere());
        assert_eq!(
            control.edges.len(),
            1,
            "control: a method on a visible type should resolve"
        );

        let out = resolve(&build(SymbolLabel::Field), nowhere());
        assert!(
            out.edges.is_empty(),
            "a call resolved to a field: {:?}",
            out.edges
        );
        assert_eq!(out.unresolved.get("a.rs"), Some(&1));
    }

    #[test]
    fn a_qualifier_matching_a_neighbouring_file_is_not_a_module_path() {
        // Without a `mod` declaration, `store.open()` on a local variable would
        // otherwise be read as a call into a neighbouring `store.rs`.
        let a = native("src/a.rs");
        let st = native("src/store.rs");
        let files = vec![
            facts(
                &a,
                vec![func("caller")],
                vec![call(&format!("{a}:caller"), "run_index", Some("store"))],
            ),
            facts(&st, vec![func("run_index")], vec![]),
        ];
        let out = resolve(&files, nowhere());
        assert!(out.edges.is_empty(), "got {:?}", out.edges);
    }

    #[test]
    fn a_module_qualified_call_to_a_module_that_does_not_exist_is_unresolved() {
        let a = native("src/a.rs");
        let idx = native("src/index.rs");
        let files = vec![
            facts(
                &a,
                vec![func("caller")],
                vec![call(&format!("{a}:caller"), "run_index", Some("elsewhere"))],
            ),
            facts(&idx, vec![func("run_index")], vec![]),
        ];
        let out = resolve(&files, nowhere());
        assert!(out.edges.is_empty(), "got {:?}", out.edges);
        assert_eq!(out.unresolved.get(a.as_str()), Some(&1));
    }

    #[test]
    fn the_suffix_tier_ignores_a_type_the_calling_file_cannot_name() {
        // Without this the tier reasons only "some project type has a method by
        // this name", which is true of a great many names the receiver never had.
        let files = vec![
            facts(
                "a.rs",
                vec![func("caller")],
                vec![call("a.rs:caller", "helper", Some("thing"))],
            ),
            facts(
                "b.rs",
                vec![
                    sym("Widget", SymbolLabel::Struct, None),
                    sym("helper", SymbolLabel::Method, Some("Widget")),
                ],
                vec![],
            ),
        ];
        let out = resolve(&files, nowhere());
        assert!(out.edges.is_empty(), "got {:?}", out.edges);
        assert_eq!(out.unresolved.get("a.rs"), Some(&1));
    }

    #[test]
    fn a_call_without_a_receiver_cannot_reach_a_method() {
        // A method needs a receiver, so `helper()` can only mean the free
        // function even though a same-named method also exists.
        let files = vec![
            facts(
                "a.rs",
                vec![func("caller")],
                vec![call("a.rs:caller", "helper", None)],
            ),
            facts(
                "b.rs",
                vec![
                    sym("Widget", SymbolLabel::Struct, None),
                    sym("helper", SymbolLabel::Method, Some("Widget")),
                ],
                vec![],
            ),
            facts("c.rs", vec![func("helper")], vec![]),
        ];
        let out = resolve(&files, nowhere());
        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].target_qualified, "c.rs:helper");
        assert_eq!(out.edges[0].resolution, Some(Resolution::UniqueName));
        assert_eq!(out.unresolved.get("a.rs"), None);
    }

    #[test]
    fn a_call_without_a_receiver_is_unresolved_when_only_a_method_matches() {
        let files = vec![
            facts(
                "a.rs",
                vec![func("caller")],
                vec![call("a.rs:caller", "helper", None)],
            ),
            facts(
                "b.rs",
                vec![
                    sym("Widget", SymbolLabel::Struct, None),
                    sym("helper", SymbolLabel::Method, Some("Widget")),
                ],
                vec![],
            ),
        ];
        let out = resolve(&files, nowhere());
        assert!(out.edges.is_empty());
        assert_eq!(out.unresolved.get("a.rs"), Some(&1));
    }

    #[test]
    fn a_module_is_never_a_call_target() {
        // `pub mod push;` shares its name with `Vec::push`; without this the
        // weak tiers attach every push in the project to the module.
        let files = vec![
            facts(
                "a.rs",
                vec![func("caller")],
                vec![
                    call("a.rs:caller", "push", None),
                    call("a.rs:caller", "push", Some("items")),
                ],
            ),
            facts(
                "mod.rs",
                vec![sym("push", SymbolLabel::Module, None)],
                vec![],
            ),
        ];
        let out = resolve(&files, nowhere());
        assert!(out.edges.is_empty(), "got {:?}", out.edges);
        assert_eq!(out.unresolved.get("a.rs"), Some(&2));
    }

    #[test]
    fn an_unplaceable_receiver_does_not_fall_back_to_the_bare_name_tiers() {
        // `row.get(0)` must not become a call to whatever single project symbol
        // happens to be named `get`, even one defined in the same file.
        let files = vec![facts(
            "a.rs",
            vec![
                func("caller"),
                sym("Store", SymbolLabel::Struct, None),
                sym("get", SymbolLabel::Function, None),
            ],
            vec![call("a.rs:caller", "get", Some("row"))],
        )];
        let out = resolve(&files, nowhere());
        assert!(out.edges.is_empty(), "got {:?}", out.edges);
        assert_eq!(out.unresolved.get("a.rs"), Some(&1));
    }

    #[test]
    fn two_types_owning_the_same_method_stay_unresolved() {
        let files = vec![
            facts(
                "a.rs",
                vec![func("caller")],
                vec![call("a.rs:caller", "helper", Some("thing"))],
            ),
            facts(
                "b.rs",
                vec![
                    sym("Widget", SymbolLabel::Struct, None),
                    sym("helper", SymbolLabel::Method, Some("Widget")),
                ],
                vec![],
            ),
            facts(
                "c.rs",
                vec![
                    sym("Gadget", SymbolLabel::Struct, None),
                    sym("helper", SymbolLabel::Method, Some("Gadget")),
                ],
                vec![],
            ),
        ];
        let out = resolve(&files, nowhere());
        assert!(out.edges.is_empty());
        assert_eq!(out.unresolved.get("a.rs"), Some(&1));
    }

    #[test]
    fn a_method_on_a_non_container_is_not_a_suffix_match() {
        // `Widget` is a plain type alias, so it owns nothing the suffix tier
        // may attach to.
        let files = vec![
            facts(
                "a.rs",
                vec![func("caller")],
                vec![call("a.rs:caller", "helper", Some("thing"))],
            ),
            facts(
                "b.rs",
                vec![
                    sym("Widget", SymbolLabel::Type, None),
                    sym("helper", SymbolLabel::Method, Some("Widget")),
                ],
                vec![],
            ),
            facts("c.rs", vec![func("helper")], vec![]),
        ];
        let out = resolve(&files, nowhere());
        assert!(out.edges.is_empty());
        assert_eq!(out.unresolved.get("a.rs"), Some(&1));
    }

    #[test]
    fn recursion_produces_no_edge_and_no_gap() {
        let files = vec![facts(
            "a.rs",
            vec![func("walk")],
            vec![call("a.rs:walk", "walk", None)],
        )];
        let out = resolve(&files, nowhere());
        assert!(
            out.edges.is_empty(),
            "a self-edge inflates fan-in for nothing"
        );
        assert!(out.unresolved.is_empty(), "a self-call did resolve");
    }

    #[test]
    fn unresolved_calls_are_counted_per_file() {
        let files = vec![
            facts(
                "a.rs",
                vec![func("caller")],
                vec![
                    call("a.rs:caller", "println", None),
                    call("a.rs:caller", "format", None),
                ],
            ),
            facts(
                "b.rs",
                vec![func("other")],
                vec![call("b.rs:other", "vec", None)],
            ),
        ];
        let out = resolve(&files, nowhere());
        assert!(out.edges.is_empty());
        assert_eq!(out.unresolved.get("a.rs"), Some(&2));
        assert_eq!(out.unresolved.get("b.rs"), Some(&1));
    }

    #[test]
    fn a_file_scope_call_yields_no_edge() {
        let files = vec![
            FileFacts {
                path: "a.rs".to_string(),
                extract: FileExtract {
                    calls: vec![RawCall {
                        caller: None,
                        callee: "helper".to_string(),
                        qualifier: None,
                        line: 3,
                    }],
                    ..Default::default()
                },
            },
            facts("b.rs", vec![func("helper")], vec![]),
        ];
        let out = resolve(&files, nowhere());
        assert!(out.edges.is_empty(), "there is no source node to attach to");
        assert!(out.unresolved.is_empty(), "the callee did resolve");
    }

    #[test]
    fn an_import_binding_outranks_a_unique_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "Cargo.toml");
        touch(root, "src/a.rs");
        touch(root, "src/b.rs");
        touch(root, "src/c.rs");

        let files = vec![
            FileFacts {
                path: native("src/a.rs"),
                extract: FileExtract {
                    symbols: vec![func("caller")],
                    calls: vec![call(
                        &format!("{}:caller", native("src/a.rs")),
                        "helper",
                        None,
                    )],
                    imports: vec![RawImport {
                        local_name: "helper".to_string(),
                        source: "crate::b::helper".to_string(),
                    }],
                    ..Default::default()
                },
            },
            facts(&native("src/b.rs"), vec![func("helper")], vec![]),
            // A second `helper` would make the unique-name tier ambiguous, so
            // an edge here can only have come from the import map.
            facts(&native("src/c.rs"), vec![func("helper")], vec![]),
        ];

        let out = resolve(&files, root);
        assert_eq!(out.edges.len(), 1);
        assert_eq!(
            out.edges[0].target_qualified,
            format!("{}:helper", native("src/b.rs"))
        );
        assert_eq!(out.edges[0].resolution, Some(Resolution::ImportMap));
    }

    /// A binary, an example or an integration test reaches its own library by
    /// the package name rather than `crate::`, and until that was recognised
    /// the whole library looked external from those files — so every call they
    /// made into it resolved to nothing. `main.rs` is usually the largest file
    /// in a crate, which is what made this worth chasing.
    #[test]
    fn a_crate_importing_itself_by_package_name_resolves_like_crate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"my-tool\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        touch(root, "src/lib.rs");
        touch(root, "src/main.rs");
        touch(root, "src/hooks.rs");

        let main = native("src/main.rs");
        let files = vec![
            FileFacts {
                path: main.clone(),
                extract: FileExtract {
                    symbols: vec![func("run")],
                    // `hooks::append(..)` — module-qualified, the shape a binary
                    // uses after `use my_tool::{hooks};`.
                    calls: vec![call(&format!("{main}:run"), "append", Some("hooks"))],
                    imports: vec![RawImport {
                        local_name: "hooks".to_string(),
                        // The dash in the package name becomes an underscore.
                        source: "my_tool".to_string(),
                    }],
                    ..Default::default()
                },
            },
            facts(&native("src/hooks.rs"), vec![func("append")], vec![]),
        ];

        let out = resolve(&files, root);
        assert_eq!(out.edges.len(), 1, "got {:?}", out.edges);
        assert_eq!(
            out.edges[0].target_qualified,
            format!("{}:append", native("src/hooks.rs"))
        );
        assert_eq!(out.edges[0].resolution, Some(Resolution::ImportMap));
    }

    /// The dangerous half of the rule above. The crate name is read from the
    /// manifest rather than inferred from "an unknown first segment", because
    /// inferring it would bind any third-party module to a same-named project
    /// file — here `tokio::fs` to `src/fs.rs`.
    #[test]
    fn another_crates_module_is_not_bound_to_a_same_named_project_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"my-tool\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        touch(root, "src/main.rs");
        touch(root, "src/fs.rs");

        let main = native("src/main.rs");
        let files = vec![
            FileFacts {
                path: main.clone(),
                extract: FileExtract {
                    symbols: vec![func("run")],
                    calls: vec![call(&format!("{main}:run"), "read", Some("fs"))],
                    imports: vec![RawImport {
                        local_name: "fs".to_string(),
                        source: "tokio".to_string(),
                    }],
                    ..Default::default()
                },
            },
            facts(&native("src/fs.rs"), vec![func("read")], vec![]),
        ];

        let out = resolve(&files, root);
        assert!(
            out.edges.is_empty(),
            "a third-party module was bound to a project file: {:?}",
            out.edges
        );
    }

    #[test]
    fn lib_section_name_wins_over_the_package_name() {
        assert_eq!(
            lib_name_from_manifest("[package]\nname = \"my-tool\"\n[lib]\nname = \"mylib\"\n")
                .as_deref(),
            Some("mylib")
        );
        assert_eq!(
            lib_name_from_manifest("[package]\nname = \"my-tool\"\n").as_deref(),
            Some("my_tool"),
            "dashes become underscores the way cargo does it"
        );
        assert_eq!(
            lib_name_from_manifest("[dependencies]\nname = \"not-a-package\"\n"),
            None,
            "a name outside [package]/[lib] is some dependency's key"
        );
    }

    /// Two files in one directory writing the same `use` must not share an
    /// answer when one of them is the directory's own module.
    #[test]
    fn the_import_cache_distinguishes_mod_rs_from_its_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "Cargo.toml");
        touch(root, "src/shared.rs");
        touch(root, "src/mcp/mod.rs");
        touch(root, "src/mcp/a.rs");
        touch(root, "src/mcp/shared.rs");

        let importer = |path: &str| FileFacts {
            path: native(path),
            extract: FileExtract {
                symbols: vec![func("caller")],
                calls: vec![call(&format!("{}:caller", native(path)), "run", None)],
                imports: vec![RawImport {
                    local_name: "run".to_string(),
                    source: "super::shared".to_string(),
                }],
                ..Default::default()
            },
        };

        let files = vec![
            importer("src/mcp/mod.rs"),
            importer("src/mcp/a.rs"),
            facts(&native("src/shared.rs"), vec![func("run")], vec![]),
            facts(&native("src/mcp/shared.rs"), vec![func("run")], vec![]),
        ];

        let out = resolve(&files, root);
        let targets: Vec<&str> = out
            .edges
            .iter()
            .map(|e| e.target_qualified.as_str())
            .collect();
        assert_eq!(
            targets,
            vec![
                format!("{}:run", native("src/shared.rs")),
                format!("{}:run", native("src/mcp/shared.rs")),
            ]
        );
    }

    #[test]
    fn a_relative_ts_import_binds_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "src/app.ts");
        touch(root, "src/util.ts");
        touch(root, "src/other.ts");

        let files = vec![
            FileFacts {
                path: native("src/app.ts"),
                extract: FileExtract {
                    symbols: vec![func("main")],
                    calls: vec![call(&format!("{}:main", native("src/app.ts")), "run", None)],
                    imports: vec![RawImport {
                        local_name: "run".to_string(),
                        source: "./util".to_string(),
                    }],
                    ..Default::default()
                },
            },
            facts(&native("src/util.ts"), vec![func("run")], vec![]),
            facts(&native("src/other.ts"), vec![func("run")], vec![]),
        ];

        let out = resolve(&files, root);
        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].resolution, Some(Resolution::ImportMap));
        assert_eq!(
            out.edges[0].target_qualified,
            format!("{}:run", native("src/util.ts"))
        );
    }

    // ---- relations --------------------------------------------------------

    #[test]
    fn relations_become_edges_without_a_resolution_tier() {
        let files = vec![
            FileFacts {
                path: "a.rs".to_string(),
                extract: FileExtract {
                    symbols: vec![sym("Derived", SymbolLabel::Class, None)],
                    relations: vec![RawRelation {
                        subject: "Derived".to_string(),
                        object: "Base".to_string(),
                        kind: EdgeKind::Inherits,
                    }],
                    ..Default::default()
                },
            },
            facts("b.rs", vec![sym("Base", SymbolLabel::Class, None)], vec![]),
        ];
        let out = resolve(&files, nowhere());
        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].source_qualified, "a.rs:Derived");
        assert_eq!(out.edges[0].target_qualified, "b.rs:Base");
        assert_eq!(out.edges[0].kind, EdgeKind::Inherits);
        assert_eq!(out.edges[0].resolution, None);
        assert_eq!(out.edges[0].line, 0);
    }

    #[test]
    fn a_relation_to_an_unknown_type_is_dropped_not_counted() {
        let files = vec![FileFacts {
            path: "a.rs".to_string(),
            extract: FileExtract {
                symbols: vec![sym("Derived", SymbolLabel::Class, None)],
                relations: vec![RawRelation {
                    subject: "Derived".to_string(),
                    object: "HttpClient".to_string(),
                    kind: EdgeKind::Implements,
                }],
                ..Default::default()
            },
        }];
        let out = resolve(&files, nowhere());
        assert!(out.edges.is_empty());
        assert!(
            out.unresolved.is_empty(),
            "the unresolved counter measures call sites only"
        );
    }

    // ---- rust module paths ------------------------------------------------

    fn rust_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "Cargo.toml");
        touch(root, "src/lib.rs");
        touch(root, "src/x.rs");
        touch(root, "src/a.rs");
        touch(root, "src/a/b.rs");
        touch(root, "src/d/mod.rs");
        touch(root, "src/d/foo.rs");
        touch(root, "src/d/x.rs");
        touch(root, "crates/inner/Cargo.toml");
        touch(root, "crates/inner/src/lib.rs");
        touch(root, "crates/inner/src/thing.rs");
        dir
    }

    fn rust(dir: &tempfile::TempDir, source: &str, from: &str) -> Option<String> {
        resolve_rust_module(source, Path::new(&native(from)), dir.path())
    }

    #[test]
    fn crate_paths_strip_item_segments_until_a_file_exists() {
        let dir = rust_fixture();
        assert_eq!(
            rust(&dir, "crate::a::b::Config", "src/lib.rs"),
            Some(native("src/a/b.rs"))
        );
        assert_eq!(
            rust(&dir, "crate::a::b", "src/lib.rs"),
            Some(native("src/a/b.rs"))
        );
        assert_eq!(
            rust(&dir, "crate::a", "src/lib.rs"),
            Some(native("src/a.rs"))
        );
        assert_eq!(
            rust(&dir, "crate::x", "src/d/foo.rs"),
            Some(native("src/x.rs"))
        );
    }

    #[test]
    fn a_directory_module_resolves_through_mod_rs() {
        let dir = rust_fixture();
        assert_eq!(
            rust(&dir, "crate::d::helper", "src/lib.rs"),
            Some(native("src/d/mod.rs"))
        );
    }

    #[test]
    fn an_item_at_the_crate_root_lands_on_lib_rs() {
        let dir = rust_fixture();
        assert_eq!(
            rust(&dir, "crate::Config", "src/d/foo.rs"),
            Some(native("src/lib.rs"))
        );
    }

    #[test]
    fn super_from_a_leaf_file_means_its_own_directory() {
        let dir = rust_fixture();
        assert_eq!(
            rust(&dir, "super::x", "src/d/foo.rs"),
            Some(native("src/d/x.rs"))
        );
    }

    #[test]
    fn super_from_mod_rs_means_the_parent_directory() {
        let dir = rust_fixture();
        assert_eq!(
            rust(&dir, "super::x", "src/d/mod.rs"),
            Some(native("src/x.rs"))
        );
    }

    #[test]
    fn super_falls_back_to_the_parent_modules_own_file() {
        let dir = rust_fixture();
        assert_eq!(
            rust(&dir, "super::Config", "src/d/foo.rs"),
            Some(native("src/d/mod.rs"))
        );
        assert_eq!(
            rust(&dir, "super", "src/d/foo.rs"),
            Some(native("src/d/mod.rs"))
        );
    }

    #[test]
    fn repeated_super_climbs_one_directory_each() {
        let dir = rust_fixture();
        assert_eq!(
            rust(&dir, "super::super::x", "src/d/foo.rs"),
            Some(native("src/x.rs"))
        );
    }

    #[test]
    fn self_resolves_within_the_same_directory() {
        let dir = rust_fixture();
        assert_eq!(
            rust(&dir, "self::x", "src/d/mod.rs"),
            Some(native("src/d/x.rs"))
        );
        assert_eq!(
            rust(&dir, "self::x", "src/d/foo.rs"),
            Some(native("src/d/x.rs"))
        );
    }

    #[test]
    fn an_external_crate_resolves_to_nothing() {
        let dir = rust_fixture();
        assert_eq!(rust(&dir, "std::fs::File", "src/lib.rs"), None);
        assert_eq!(rust(&dir, "serde::Deserialize", "src/lib.rs"), None);
        assert_eq!(rust(&dir, "tokio", "src/d/foo.rs"), None);
    }

    #[test]
    fn crate_is_the_nearest_cargo_package_not_the_project_root() {
        let dir = rust_fixture();
        assert_eq!(
            rust(&dir, "crate::thing", "crates/inner/src/lib.rs"),
            Some(native("crates/inner/src/thing.rs"))
        );
    }

    #[test]
    fn a_file_outside_any_cargo_package_has_no_crate_root() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "src/lib.rs");
        assert_eq!(
            resolve_rust_module("crate::x", Path::new(&native("src/lib.rs")), dir.path()),
            None
        );
    }

    // ---- js/ts relative imports -------------------------------------------

    #[test]
    fn relative_specifiers_try_each_extension_then_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "src/app.ts");
        touch(root, "src/util.ts");
        touch(root, "src/widget.tsx");
        touch(root, "src/legacy.js");
        touch(root, "src/bundle/index.ts");
        touch(root, "other/shared.js");

        let from = native("src/app.ts");
        let from = Path::new(&from);
        let go = |spec: &str| resolve_relative_module(spec, from, root);

        assert_eq!(go("./util"), Some(native("src/util.ts")));
        assert_eq!(go("./widget"), Some(native("src/widget.tsx")));
        assert_eq!(go("./legacy"), Some(native("src/legacy.js")));
        assert_eq!(go("./bundle"), Some(native("src/bundle/index.ts")));
        assert_eq!(go("../other/shared"), Some(native("other/shared.js")));
        assert_eq!(go("./util.ts"), Some(native("src/util.ts")));
        assert_eq!(go("./missing"), None);
    }

    #[test]
    fn a_bare_specifier_is_a_package_not_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "src/app.ts");
        touch(root, "src/react.ts");

        let from = native("src/app.ts");
        assert_eq!(
            resolve_relative_module("react", Path::new(&from), root),
            None,
            "a same-named local file must not capture a package import"
        );
    }

    // ---- path plumbing ----------------------------------------------------

    #[test]
    fn normalize_is_lexical_and_keeps_leading_parents() {
        assert_eq!(normalize(Path::new("a/./b/../c")), PathBuf::from("a/c"));
        assert_eq!(
            normalize(Path::new("../../a")),
            PathBuf::from("..").join("..").join("a")
        );
        assert_eq!(normalize(Path::new("./a")), PathBuf::from("a"));
    }

    #[test]
    fn only_rejects_ambiguity() {
        assert_eq!(only([1].into_iter()), Some(1));
        assert_eq!(only([1, 2].into_iter()), None);
        assert_eq!(only(std::iter::empty::<i32>()), None);
    }
}

#[cfg(test)]
mod stoplist_tests {
    use super::is_ubiquitous_method;

    #[test]
    fn the_stop_list_is_sorted_for_binary_search() {
        // `binary_search` silently returns wrong answers on unsorted input.
        assert!(
            super::COMMON_METHOD_NAMES.windows(2).all(|w| w[0] < w[1]),
            "stop list must stay sorted and free of duplicates"
        );
        for name in [
            "as_str", "clone", "get", "is_empty", "ok", "push", "unwrap", "write",
        ] {
            assert!(is_ubiquitous_method(name), "{name} should be excluded");
        }
        for name in [
            "deprecate_removed",
            "rebuild_edges",
            "resolve_qualified",
            "zzz",
        ] {
            assert!(!is_ubiquitous_method(name), "{name} should be allowed");
        }
    }
}
