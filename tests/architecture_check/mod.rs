#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::visit::Visit;
use syn::{Item, Meta, UseTree};

mod tests;

const PRODUCTION_LIMIT: usize = 1_598;
const TEST_LIMIT: usize = 2_098;
const INVENTORY_PATH: &str = "tests/architecture-exclusions.txt";

#[test]
fn repository_rust_sources_follow_architecture_rules() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inventory_path = root.join(INVENTORY_PATH);
    let inventory_text = fs::read_to_string(&inventory_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", inventory_path.display()));
    let exclusions =
        parse_exclusion_inventory(root, &inventory_text).unwrap_or_else(|error| panic!("{error}"));
    let sources =
        discover_rust_sources(root, &exclusions).unwrap_or_else(|error| panic!("{error}"));
    let crate_roots =
        discover_crate_roots(root, &sources).unwrap_or_else(|error| panic!("{error}"));

    for path in sources {
        validate_rust_file_with_roots(root, &path, &crate_roots)
            .unwrap_or_else(|error| panic!("{error}"));
    }
}

fn discover_rust_sources(
    root: &Path,
    exclusions: &HashSet<PathBuf>,
) -> Result<Vec<PathBuf>, String> {
    let mut pending = vec![root.to_path_buf()];
    let mut sources = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("{}: discover Rust files: {error}", directory.display()))?;
        let mut paths = entries
            .map(|entry| {
                entry.map(|entry| entry.path()).map_err(|error| {
                    format!("{}: read directory entry: {error}", directory.display())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        paths.sort();
        for path in paths.into_iter().rev() {
            let relative = path.strip_prefix(root).map_err(|error| {
                format!(
                    "{}: repository-relative discovery path: {error}",
                    path.display()
                )
            })?;
            if relative == Path::new(".git") || relative == Path::new("target") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                format!("{}: inspect discovered path: {error}", relative.display())
            })?;
            if metadata.file_type().is_symlink() {
                reject_or_skip_symlink(&path, relative)?;
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file()
                && path.extension().is_some_and(|extension| extension == "rs")
                && !exclusions.contains(relative)
            {
                sources.push(path);
            }
        }
    }
    sources.sort();
    Ok(sources)
}

fn reject_or_skip_symlink(path: &Path, relative: &Path) -> Result<(), String> {
    if path.extension().is_some_and(|extension| extension == "rs") {
        return Err(format!(
            "{}: symlink rule forbids Rust source symlink aliases",
            relative.display()
        ));
    }
    let target = fs::metadata(path).map_err(|error| {
        format!(
            "{}: symlink rule cannot classify target: {error}",
            relative.display()
        )
    })?;
    if target.is_dir() {
        return Err(format!(
            "{}: symlink rule forbids directory symlinks",
            relative.display()
        ));
    }
    Ok(())
}

fn parse_exclusion_inventory(root: &Path, text: &str) -> Result<HashSet<PathBuf>, String> {
    let mut exclusions = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        let inventory_line = index + 1;
        if fields.len() != 3 {
            return Err(format!(
                "{INVENTORY_PATH}:{inventory_line}: exclusion inventory requires exactly three tab-separated fields"
            ));
        }
        if !matches!(fields[0], "generated" | "vendored") {
            return Err(format!(
                "{INVENTORY_PATH}:{inventory_line}: exclusion inventory kind must be generated or vendored"
            ));
        }
        let relative = Path::new(fields[1]);
        if relative.as_os_str().is_empty()
            || relative.is_absolute()
            || relative
                .extension()
                .is_none_or(|extension| extension != "rs")
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            || fields[1].contains('\\')
            || fields[1]
                .split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
            || relative.to_string_lossy() != fields[1]
        {
            return Err(format!(
                "{INVENTORY_PATH}:{inventory_line}: exclusion inventory path must be a canonical repository-relative .rs file path"
            ));
        }
        if fields[2].trim().is_empty() {
            return Err(format!(
                "{INVENTORY_PATH}:{inventory_line}: exclusion inventory requires provenance"
            ));
        }
        if !exclusions.insert(relative.to_path_buf()) {
            return Err(format!(
                "{INVENTORY_PATH}:{inventory_line}: duplicate exclusion inventory path {}",
                relative.display()
            ));
        }
        let absolute = root.join(relative);
        let metadata = fs::symlink_metadata(&absolute).map_err(|error| {
            format!(
                "{INVENTORY_PATH}:{inventory_line}: stale exclusion entry {}: {error}",
                relative.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "{INVENTORY_PATH}:{inventory_line}: exclusion entry {} must be a regular file, not a symlink or directory",
                relative.display()
            ));
        }
    }
    Ok(exclusions)
}

fn validate_rust_file(root: &Path, path: &Path) -> Result<(), String> {
    let sources = vec![path.to_path_buf()];
    let crate_roots = discover_crate_roots(root, &sources)?;
    validate_rust_file_with_roots(root, path, &crate_roots)
}

fn validate_rust_file_with_roots(
    root: &Path,
    path: &Path,
    crate_roots: &HashSet<PathBuf>,
) -> Result<(), String> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let bytes = fs::read(path)
        .map_err(|error| format!("{}: read Rust source: {error}", relative.display()))?;
    let line_count = physical_line_count(&bytes);
    let is_test = relative.file_name().is_some_and(|name| name == "tests.rs")
        || relative.starts_with(Path::new("tests"));
    let limit = if is_test {
        TEST_LIMIT
    } else {
        PRODUCTION_LIMIT
    };
    if line_count > limit {
        return Err(format!(
            "{}: source-file line limit exceeded: measured {line_count} physical lines, limit {limit}",
            relative.display()
        ));
    }
    let source = std::str::from_utf8(&bytes)
        .map_err(|error| format!("{}: Rust source must be UTF-8: {error}", relative.display()))?;
    let syntax = syn::parse_file(source)
        .map_err(|error| format!("{}: Rust syntax parse failed: {error}", relative.display()))?;
    validate_recursive_syntax(relative, &syntax)?;
    validate_items(root, path, &syntax.items, crate_roots)
}

fn validate_recursive_syntax(relative: &Path, syntax: &syn::File) -> Result<(), String> {
    let mut visitor = ProhibitedSyntaxVisitor { violation: None };
    visitor.visit_file(syntax);
    if let Some(rule) = visitor.violation {
        return Err(rule_error(relative, rule));
    }
    Ok(())
}

struct ProhibitedSyntaxVisitor {
    violation: Option<&'static str>,
}

impl<'ast> Visit<'ast> for ProhibitedSyntaxVisitor {
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if self.violation.is_some() {
            return;
        }
        if module
            .attrs
            .iter()
            .any(|attribute| meta_contains_path(&attribute.meta))
        {
            self.violation = Some("#[path] module relocation is forbidden");
        } else if module.content.is_some() {
            self.violation = Some("inline module bodies are forbidden");
        } else {
            syn::visit::visit_item_mod(self, module);
        }
    }

    fn visit_item_use(&mut self, import: &'ast syn::ItemUse) {
        if self.violation.is_some() {
            return;
        }
        if use_tree_has_glob(&import.tree) {
            self.violation = Some("glob imports are forbidden");
        } else {
            syn::visit::visit_item_use(self, import);
        }
    }
}

fn physical_line_count(bytes: &[u8]) -> usize {
    bytes.split_inclusive(|byte| *byte == b'\n').count()
}

fn discover_crate_roots(root: &Path, sources: &[PathBuf]) -> Result<HashSet<PathBuf>, String> {
    let mut roots = HashSet::new();
    let manifest_path = root.join("Cargo.toml");
    let has_manifest = manifest_path.is_file();
    if has_manifest {
        let manifest_text = fs::read_to_string(&manifest_path)
            .map_err(|error| format!("Cargo.toml: read target configuration: {error}"))?;
        let manifest = toml::from_str::<toml::Table>(&manifest_text)
            .map_err(|error| format!("Cargo.toml: parse target configuration: {error}"))?;
        add_manifest_target_paths(root, &manifest, &mut roots)?;
    }

    for relative in ["src/lib.rs", "src/main.rs", "lib.rs", "main.rs"] {
        let path = root.join(relative);
        if path.is_file() {
            roots.insert(path);
        }
    }
    if !has_manifest {
        let build_script = root.join("build.rs");
        if build_script.is_file() {
            roots.insert(build_script);
        }
    }
    for path in sources {
        let relative = path.strip_prefix(root).unwrap_or(path);
        let components = relative.components().collect::<Vec<_>>();
        let Some(first) = components.first() else {
            continue;
        };
        let is_auto_target = matches!(
            first.as_os_str().to_str(),
            Some("tests" | "examples" | "benches")
        ) && (components.len() == 2
            || (components.len() == 3 && components[2].as_os_str() == "main.rs"));
        let is_binary_target = relative.starts_with("src/bin")
            && (components.len() == 3
                || (components.len() == 4 && components[3].as_os_str() == "main.rs"));
        if is_auto_target || is_binary_target {
            roots.insert(path.clone());
        }
    }
    Ok(roots)
}

fn add_manifest_target_paths(
    root: &Path,
    manifest: &toml::Table,
    roots: &mut HashSet<PathBuf>,
) -> Result<(), String> {
    let mut configured = Vec::new();
    if let Some(package) = manifest.get("package").and_then(toml::Value::as_table) {
        match package.get("build") {
            Some(toml::Value::String(path)) => configured.push(path.as_str()),
            Some(toml::Value::Boolean(false)) => {}
            Some(toml::Value::Boolean(true)) | None => {
                let default_build_script = root.join("build.rs");
                if default_build_script.is_file() {
                    roots.insert(default_build_script);
                }
            }
            Some(_) => {
                return Err("Cargo.toml: package.build must be a path or boolean".to_owned());
            }
        }
    }
    for section in ["lib", "bin", "example", "test", "bench"] {
        if let Some(targets) = manifest.get(section) {
            let targets = match targets {
                toml::Value::Array(entries) => entries.iter().collect::<Vec<_>>(),
                target => vec![target],
            };
            for target in targets {
                if let Some(path) = target.get("path").and_then(toml::Value::as_str) {
                    configured.push(path);
                }
            }
        }
    }
    for path in configured {
        let target = root.join(path);
        if !target.is_file() {
            return Err(format!(
                "Cargo.toml: configured Rust target {path} is not a file"
            ));
        }
        roots.insert(target);
    }
    Ok(())
}

fn validate_items(
    root: &Path,
    parent_path: &Path,
    items: &[Item],
    crate_roots: &HashSet<PathBuf>,
) -> Result<(), String> {
    let relative = parent_path.strip_prefix(root).unwrap_or(parent_path);
    let child_names = declared_children(items);
    let child_usage = child_macro_usage(root, parent_path, relative, items, crate_roots)?;
    validate_item_order(
        root,
        parent_path,
        relative,
        items,
        &child_names,
        &child_usage,
        crate_roots,
    )
}

fn meta_contains_path(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("path"),
        Meta::List(list) if list.path.is_ident("cfg_attr") => {
            Punctuated::<Meta, Comma>::parse_terminated
                .parse2(list.tokens.clone())
                .is_ok_and(|attributes| attributes.iter().skip(1).any(meta_contains_path))
        }
        Meta::List(_) => false,
        Meta::NameValue(value) => value.path.is_ident("path"),
    }
}

fn declared_children(items: &[Item]) -> HashSet<String> {
    items
        .iter()
        .filter_map(|item| match item {
            Item::Mod(module) if module.content.is_none() => Some(module.ident.to_string()),
            _ => None,
        })
        .collect()
}

fn child_macro_usage(
    root: &Path,
    parent_path: &Path,
    relative: &Path,
    items: &[Item],
    crate_roots: &HashSet<PathBuf>,
) -> Result<HashMap<String, HashSet<String>>, String> {
    let mut usage = HashMap::new();
    for item in items {
        if let Item::Mod(module) = item
            && module.content.is_none()
        {
            let module_path =
                resolve_module_file(root, parent_path, module, relative, crate_roots)?;
            let mut invocations = HashSet::new();
            collect_module_macro_invocations(
                root,
                &module_path,
                crate_roots,
                &mut HashSet::new(),
                &mut invocations,
            )?;
            usage.insert(module.ident.to_string(), invocations);
        }
    }
    Ok(usage)
}

fn validate_item_order(
    root: &Path,
    parent_path: &Path,
    relative: &Path,
    items: &[Item],
    child_names: &HashSet<String>,
    child_usage: &HashMap<String, HashSet<String>>,
    crate_roots: &HashSet<PathBuf>,
) -> Result<(), String> {
    let mut phase = ItemPhase::Imports;
    let mut prior_category = 0_usize;
    for (index, item) in items.iter().enumerate() {
        match item {
            Item::Use(import) => {
                if phase != ItemPhase::Imports {
                    return Err(rule_error(
                        relative,
                        "imports must precede module declarations and other items",
                    ));
                }
                validate_import(relative, &import.tree, child_names, &mut prior_category)?;
            }
            Item::Mod(module) => {
                if phase == ItemPhase::Other {
                    return Err(rule_error(
                        relative,
                        "module declarations must precede other module-level items",
                    ));
                }
                phase = ItemPhase::Modules;
                let module_path =
                    resolve_module_file(root, parent_path, module, relative, crate_roots)?;
                reject_noncanonical_module_root(root, &module_path, relative)?;
            }
            Item::Macro(mac)
                if is_macro_rules(&mac.mac.path)
                    && matches!(
                        phase,
                        ItemPhase::Imports | ItemPhase::Macro | ItemPhase::Modules
                    ) =>
            {
                let name = mac
                    .ident
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                let later_child_modules = items[index + 1..]
                    .iter()
                    .filter_map(|candidate| match candidate {
                        Item::Mod(module) if module.content.is_none() => {
                            Some(module.ident.to_string())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let used_by_later_child = later_child_modules.iter().any(|child| {
                    child_usage
                        .get(child)
                        .is_some_and(|invocations| invocations.contains(&name))
                });
                if !later_child_modules.is_empty() && !used_by_later_child {
                    return Err(rule_error(
                        relative,
                        "only a macro_rules! macro required by a later child module may precede that declaration",
                    ));
                }
                if phase != ItemPhase::Modules {
                    phase = ItemPhase::Macro;
                }
            }
            _ => phase = ItemPhase::Other,
        }
    }
    Ok(())
}

fn validate_import(
    relative: &Path,
    tree: &UseTree,
    child_names: &HashSet<String>,
    prior_category: &mut usize,
) -> Result<(), String> {
    let mut categories = BTreeSet::new();
    let mut paths = Vec::new();
    collect_use_paths(tree, &mut Vec::new(), &mut paths);
    for path in &paths {
        if path
            .first()
            .is_some_and(|ident| child_names.contains(&ident.to_string()))
        {
            return Err(rule_error(relative, "sibling import requires self::"));
        }
        categories.insert(import_category(path));
    }
    if categories.len() > 1 {
        return Err(rule_error(
            relative,
            "import-group ordering requires separate standard, external, and crate-local groups",
        ));
    }
    if let Some(category) = categories.first().copied() {
        if category < *prior_category {
            return Err(rule_error(
                relative,
                "import-group ordering requires standard, external, and crate-local groups in order",
            ));
        }
        *prior_category = category;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ItemPhase {
    Imports,
    Macro,
    Modules,
    Other,
}

fn rule_error(path: &Path, rule: &str) -> String {
    format!("{}: {rule}", path.display())
}

fn import_category(path: &[syn::Ident]) -> usize {
    match path.first().map(ToString::to_string).as_deref() {
        Some("std" | "core" | "alloc") => 0,
        Some("crate" | "self" | "super") => 2,
        _ => 1,
    }
}

fn collect_use_paths(
    tree: &UseTree,
    prefix: &mut Vec<syn::Ident>,
    paths: &mut Vec<Vec<syn::Ident>>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.clone());
            collect_use_paths(&path.tree, prefix, paths);
            prefix.pop();
        }
        UseTree::Name(name) => {
            prefix.push(name.ident.clone());
            paths.push(prefix.clone());
            prefix.pop();
        }
        UseTree::Rename(rename) => {
            prefix.push(rename.ident.clone());
            paths.push(prefix.clone());
            prefix.pop();
        }
        UseTree::Glob(_) => paths.push(prefix.clone()),
        UseTree::Group(group) => {
            for tree in &group.items {
                collect_use_paths(tree, prefix, paths);
            }
        }
    }
}

fn use_tree_has_glob(tree: &UseTree) -> bool {
    match tree {
        UseTree::Glob(_) => true,
        UseTree::Path(path) => use_tree_has_glob(&path.tree),
        UseTree::Group(group) => group.items.iter().any(use_tree_has_glob),
        UseTree::Name(_) | UseTree::Rename(_) => false,
    }
}

fn is_macro_rules(path: &syn::Path) -> bool {
    path.is_ident("macro_rules")
}

fn resolve_module_file(
    root: &Path,
    parent_path: &Path,
    module: &syn::ItemMod,
    relative_parent: &Path,
    crate_roots: &HashSet<PathBuf>,
) -> Result<PathBuf, String> {
    let parent = parent_path.parent().unwrap_or_else(|| Path::new(""));
    let directory = if crate_roots.contains(parent_path)
        || parent_path.file_name().is_some_and(|name| name == "mod.rs")
    {
        parent.to_path_buf()
    } else {
        parent.join(parent_path.file_stem().unwrap_or_default())
    };
    let name = module.ident.to_string();
    let flat = directory.join(format!("{name}.rs"));
    let nested = directory.join(&name).join("mod.rs");
    let flat_exists = flat.is_file();
    let nested_exists = nested.is_file();
    if name == "tests" && nested_exists {
        return Err(rule_error(
            relative_parent,
            &format!(
                "unit-test module tests must use its sibling {} file; nested module root {} is forbidden",
                flat.strip_prefix(root).unwrap_or(&flat).display(),
                nested.strip_prefix(root).unwrap_or(&nested).display()
            ),
        ));
    }
    if flat_exists && nested_exists {
        return Err(rule_error(
            relative_parent,
            &format!(
                "ambiguous/noncanonical module roots for {name}: both {} and {} exist",
                flat.strip_prefix(root).unwrap_or(&flat).display(),
                nested.strip_prefix(root).unwrap_or(&nested).display()
            ),
        ));
    }
    match (flat_exists, nested_exists) {
        (true, false) => Ok(flat),
        (false, true) => Ok(nested),
        (false, false) => Err(rule_error(
            relative_parent,
            &format!("cannot resolve filesystem-backed module {name}"),
        )),
        (true, true) => Err(rule_error(relative_parent, "ambiguous module root")),
    }
}

fn reject_noncanonical_module_root(
    root: &Path,
    module_path: &Path,
    parent: &Path,
) -> Result<(), String> {
    let source = fs::read_to_string(module_path).map_err(|error| {
        format!(
            "{}: read module for canonical-root check: {error}",
            module_path
                .strip_prefix(root)
                .unwrap_or(module_path)
                .display()
        )
    })?;
    let syntax = syn::parse_file(&source).map_err(|error| {
        format!(
            "{}: parse module for canonical-root check: {error}",
            module_path
                .strip_prefix(root)
                .unwrap_or(module_path)
                .display()
        )
    })?;
    if module_path.file_name().is_some_and(|name| name != "mod.rs")
        && syntax
            .items
            .iter()
            .any(|item| matches!(item, Item::Mod(module) if module.content.is_none()))
    {
        let relative = module_path.strip_prefix(root).unwrap_or(module_path);
        return Err(rule_error(
            parent,
            &format!(
                "noncanonical module root {} declares children; use its directory mod.rs root",
                relative.display()
            ),
        ));
    }
    Ok(())
}

fn collect_module_macro_invocations(
    root: &Path,
    path: &Path,
    crate_roots: &HashSet<PathBuf>,
    visited: &mut HashSet<PathBuf>,
    invocations: &mut HashSet<String>,
) -> Result<(), String> {
    if !visited.insert(path.to_path_buf()) {
        return Ok(());
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    let source = fs::read_to_string(path).map_err(|error| {
        format!(
            "{}: read child module for macro usage: {error}",
            relative.display()
        )
    })?;
    let syntax = syn::parse_file(&source).map_err(|error| {
        format!(
            "{}: parse child module for macro usage: {error}",
            relative.display()
        )
    })?;
    let mut visitor = MacroInvocationVisitor::default();
    visitor.visit_file(&syntax);
    invocations.extend(visitor.invocations);
    for item in &syntax.items {
        if let Item::Mod(module) = item
            && module.content.is_none()
        {
            let child = resolve_module_file(root, path, module, relative, crate_roots)?;
            collect_module_macro_invocations(root, &child, crate_roots, visited, invocations)?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct MacroInvocationVisitor {
    invocations: HashSet<String>,
}

impl<'ast> Visit<'ast> for MacroInvocationVisitor {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if let Some(segment) = mac.path.segments.last() {
            self.invocations.insert(segment.ident.to_string());
        }
        syn::visit::visit_macro(self, mac);
    }
}
