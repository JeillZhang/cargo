use cargo_util_schemas::manifest;
use cargo_util_terminal::report::AnnotationKind;
use cargo_util_terminal::report::Group;
use cargo_util_terminal::report::Level;
use cargo_util_terminal::report::Origin;
use cargo_util_terminal::report::Patch;
use cargo_util_terminal::report::Snippet;
use indexmap::IndexMap;
use indexmap::IndexSet;
use tracing::{debug, instrument, trace};

use super::BuildRunner;
use super::unit::Unit;
use crate::core::Dependency;
use crate::core::Package;
use crate::core::PackageId;
use crate::core::compiler::build_config::CompileMode;
use crate::core::dependency::DepKind;
use crate::core::manifest::TargetKind;
use crate::lints::LintLevel;
use crate::lints::get_key_value_span;
use crate::lints::rel_cwd_manifest_path;
use crate::lints::rules::unused_dependencies::LINT;
use crate::util::errors::CargoResult;
use crate::util::interning::InternedString;

/// Track and translate `unused_externs` to `unused_dependencies`
pub struct UnusedDepState {
    states: IndexMap<PackageId, IndexMap<DepKind, DependenciesState>>,
}

impl UnusedDepState {
    #[instrument(name = "UnusedDepState::new", skip_all)]
    pub fn new(build_runner: &mut BuildRunner<'_, '_>) -> Self {
        // Find all units for a package that can report unused externs
        let mut root_build_script_builds = IndexSet::new();
        let roots = &build_runner.bcx.roots;
        for root in roots.iter() {
            for build_script_run in build_runner.unit_deps(root).iter() {
                if !build_script_run.unit.target.is_custom_build()
                    && build_script_run.unit.pkg.package_id() != root.pkg.package_id()
                {
                    continue;
                }
                for build_script_build in build_runner.unit_deps(&build_script_run.unit).iter() {
                    if !build_script_build.unit.target.is_custom_build()
                        && build_script_build.unit.pkg.package_id() != root.pkg.package_id()
                    {
                        continue;
                    }
                    if build_script_build.unit.mode != CompileMode::Build {
                        continue;
                    }
                    root_build_script_builds.insert(build_script_build.unit.clone());
                }
            }
        }

        trace!(
            "selected dep kinds: {:?}",
            build_runner.bcx.selected_dep_kinds
        );
        let mut states = IndexMap::<_, IndexMap<_, DependenciesState>>::new();
        for root in roots.iter().chain(root_build_script_builds.iter()) {
            let pkg_id = root.pkg.package_id();
            let dep_kind = dep_kind_of(root);
            if !build_runner.bcx.selected_dep_kinds.contains(dep_kind) {
                trace!(
                    "pkg {} v{} ({dep_kind:?}): ignoring unused deps due to non-exhaustive units",
                    pkg_id.name(),
                    pkg_id.version(),
                );
                continue;
            }
            trace!(
                "tracking root {} {} ({:?})",
                root.pkg.name(),
                unit_desc(root),
                dep_kind
            );

            let state = states
                .entry(pkg_id)
                .or_default()
                .entry(dep_kind)
                .or_default();
            *state.needed_units.get_or_insert_default() += 1;
            for dep in build_runner.unit_deps(root).iter() {
                trace!(
                    "    => {} (deps={})",
                    dep.unit.pkg.name(),
                    dep.manifest_deps.0.is_some()
                );
                let manifest_deps = if let Some(manifest_deps) = &dep.manifest_deps.0 {
                    Some(manifest_deps.clone())
                } else if dep.unit.pkg.package_id() == root.pkg.package_id() {
                    None
                } else {
                    continue;
                };
                state.externs.insert(
                    dep.extern_crate_name,
                    ExternState {
                        unit: dep.unit.clone(),
                        manifest_deps,
                    },
                );
            }
        }

        Self { states }
    }

    pub fn record_unused_externs_for_unit(&mut self, unit: &Unit, unused_externs: Vec<String>) {
        let pkg_id = unit.pkg.package_id();
        let dep_kind = dep_kind_of(unit);
        trace!(
            "pkg {} v{} ({dep_kind:?}): unused externs {unused_externs:?}",
            pkg_id.name(),
            pkg_id.version(),
        );
        let state = self
            .states
            .entry(pkg_id)
            .or_default()
            .entry(dep_kind)
            .or_default();
        state
            .unused_externs
            .entry(unit.clone())
            .or_default()
            .extend(unused_externs.into_iter().map(|s| InternedString::new(&s)));
    }

    #[instrument(skip_all)]
    pub fn emit_unused_warnings(
        &self,
        warn_count: &mut usize,
        error_count: &mut usize,
        build_runner: &mut BuildRunner<'_, '_>,
    ) -> CargoResult<()> {
        for (pkg_id, states) in &self.states {
            let Some(pkg) = self.get_package(pkg_id) else {
                continue;
            };
            let toml_lints = pkg
                .manifest()
                .normalized_toml()
                .lints
                .clone()
                .map(|lints| lints.lints)
                .unwrap_or(manifest::TomlLints::default());
            let cargo_lints = toml_lints
                .get("cargo")
                .cloned()
                .unwrap_or(manifest::TomlToolLints::default());
            let (lint_level, reason) = LINT.level(
                &cargo_lints,
                pkg.rust_version(),
                pkg.manifest().unstable_features(),
            );

            if lint_level == LintLevel::Allow {
                for (dep_kind, state) in states.iter() {
                    for ext in state.unused_externs.values().flatten() {
                        debug!(
                            "pkg {} v{} ({dep_kind:?}): ignoring unused extern `{ext}`, lint is allowed",
                            pkg_id.name(),
                            pkg_id.version(),
                        );
                    }
                }
                continue;
            }

            let manifest_path = rel_cwd_manifest_path(pkg.manifest_path(), build_runner.bcx.gctx);
            let mut lint_count = 0;
            for (dep_kind, state) in states.iter() {
                let Some(needed_units) = state.needed_units else {
                    // not one we care to report
                    for ext in state.unused_externs.values().flatten() {
                        debug!(
                            "pkg {} v{} ({dep_kind:?}): ignoring unused extern `{ext}`, untracked dependent",
                            pkg_id.name(),
                            pkg_id.version(),
                        );
                    }
                    continue;
                };
                if state.unused_externs.len() != needed_units {
                    // Some compilations errored without printing the unused externs.
                    // Don't print the warning in order to reduce false positive
                    // spam during errors.
                    for ext in state.unused_externs.values().flatten() {
                        debug!(
                            "pkg {} v{} ({dep_kind:?}): ignoring unused extern `{ext}`, {} outstanding units",
                            pkg_id.name(),
                            pkg_id.version(),
                            needed_units - state.unused_externs.len()
                        );
                    }
                    continue;
                }

                for (ext, extern_state) in &state.externs {
                    if state
                        .unused_externs
                        .values()
                        .any(|unused| !unused.contains(ext))
                    {
                        trace!(
                            "pkg {} v{} ({dep_kind:?}): extern {} is used",
                            pkg_id.name(),
                            pkg_id.version(),
                            ext
                        );
                        continue;
                    }
                    if is_transitive_dep(&extern_state.unit, &state.unused_externs, build_runner) {
                        debug!(
                            "pkg {} v{} ({dep_kind:?}): ignoring unused extern `{ext}`, may be activating features",
                            pkg_id.name(),
                            pkg_id.version(),
                        );
                        continue;
                    }

                    // Implicitly added dependencies (in the same crate) aren't interesting
                    let dependency = if let Some(dependency) = &extern_state.manifest_deps {
                        dependency
                    } else {
                        continue;
                    };
                    for dependency in dependency {
                        let manifest = pkg.manifest();
                        let document = manifest.document();
                        let contents = manifest.contents();
                        let level = lint_level.to_diagnostic_level();
                        let emitted_source = LINT.emitted_source(lint_level, reason);
                        let toml_path = dependency.toml_path();

                        let mut primary = Group::with_title(level.primary_title(LINT.desc));
                        if let Some(document) = document
                            && let Some(contents) = contents
                            && let Some(span) = get_key_value_span(document, &toml_path)
                        {
                            let span = span.key.start..span.value.end;
                            primary = primary.element(
                                Snippet::source(contents)
                                    .path(&manifest_path)
                                    .annotation(AnnotationKind::Primary.span(span)),
                            );
                        } else {
                            primary = primary.element(Origin::path(&manifest_path));
                        }
                        if lint_count == 0 {
                            primary = primary.element(Level::NOTE.message(emitted_source));
                        }
                        lint_count += 1;
                        let mut report = vec![primary];
                        if let Some(document) = document
                            && let Some(contents) = contents
                            && let Some(span) = get_key_value_span(document, &toml_path)
                        {
                            let span = span.key.start..span.value.end;
                            let mut help = Group::with_title(
                                Level::HELP.secondary_title("remove the dependency"),
                            );
                            help = help.element(
                                Snippet::source(contents)
                                    .path(&manifest_path)
                                    .patch(Patch::new(span, "")),
                            );
                            report.push(help);
                        }

                        if lint_level.is_warn() {
                            *warn_count += 1;
                        }
                        if lint_level.is_error() {
                            *error_count += 1;
                        }
                        build_runner
                            .bcx
                            .gctx
                            .shell()
                            .print_report(&report, lint_level.force())?;
                    }
                }
            }
        }
        Ok(())
    }

    fn get_package(&self, pkg_id: &PackageId) -> Option<&Package> {
        let state = self.states.get(pkg_id)?;
        let mut iter = state.values();
        let state = iter.next()?;
        let mut iter = state.unused_externs.keys();
        let unit = iter.next()?;
        Some(&unit.pkg)
    }
}

/// Track a package's [`DepKind`]
#[derive(Default)]
struct DependenciesState {
    /// All declared dependencies
    externs: IndexMap<InternedString, ExternState>,
    /// Expected [`Self::unused_externs`] entries to know we've received them all
    ///
    /// To avoid warning in cases where we didn't,
    /// e.g. if a [`Unit`] errored and didn't report unused externs.
    needed_units: Option<usize>,
    /// As reported by rustc
    unused_externs: IndexMap<Unit, Vec<InternedString>>,
}

#[derive(Clone)]
struct ExternState {
    unit: Unit,
    manifest_deps: Option<Vec<Dependency>>,
}

fn dep_kind_of(unit: &Unit) -> DepKind {
    match unit.target.kind() {
        TargetKind::Lib(_) => match unit.mode {
            // To support lib.rs with #[cfg(test)] use foo_crate as _;
            CompileMode::Test => DepKind::Development,
            _ => DepKind::Normal,
        },
        TargetKind::Bin => DepKind::Normal,
        TargetKind::Test => DepKind::Development,
        TargetKind::Bench => DepKind::Development,
        TargetKind::ExampleLib(_) => DepKind::Development,
        TargetKind::ExampleBin => DepKind::Development,
        TargetKind::CustomBuild => DepKind::Build,
    }
}

fn unit_desc(unit: &Unit) -> String {
    format!(
        "{}/{}+{:?}",
        unit.target.name(),
        unit.target.kind().description(),
        unit.mode,
    )
}

#[instrument(skip_all)]
fn is_transitive_dep(
    direct_dep_unit: &Unit,
    unused_externs: &IndexMap<Unit, Vec<InternedString>>,
    build_runner: &mut BuildRunner<'_, '_>,
) -> bool {
    let mut queue = std::collections::VecDeque::new();
    for root_unit in unused_externs.keys() {
        for unit_dep in build_runner.unit_deps(root_unit) {
            if root_unit.pkg.package_id() == unit_dep.unit.pkg.package_id() {
                continue;
            }
            if unit_dep.unit == *direct_dep_unit {
                continue;
            }
            queue.push_back(&unit_dep.unit);
        }
    }

    while let Some(dep_unit) = queue.pop_front() {
        for unit_dep in build_runner.unit_deps(dep_unit) {
            if unit_dep.unit == *direct_dep_unit {
                return true;
            }
            queue.push_back(&unit_dep.unit);
        }
    }

    false
}
