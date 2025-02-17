use std::collections::{BTreeMap, BTreeSet};

use anyhow::anyhow;
use cargo_raze::context::{
    BuildableDependency, CrateContext, CrateDependencyContext, CrateTargetedDepContext,
};
use cargo_raze::util::get_matching_bazel_triples;
use cfg_expr::targets::get_builtin_target_by_triple;
use itertools::Itertools;
use semver::Version;

use crate::{
    renderer::{RenderConfig, Renderer},
    resolver::Dependencies,
};

#[derive(Debug, Default)]
pub struct ConsolidatorOverride {
    // Mapping of environment variables key -> value.
    pub extra_rustc_env_vars: BTreeMap<String, String>,
    // Mapping of environment variables key -> value.
    pub extra_build_script_env_vars: BTreeMap<String, String>,
    // Mapping of target triple or spec -> extra bazel target dependencies.
    pub extra_bazel_deps: BTreeMap<String, Vec<String>>,
    // Mapping of target triple or spec -> extra bazel target data dependencies.
    pub extra_bazel_data_deps: BTreeMap<String, Vec<String>>,
    // Mapping of target triple or spec -> extra bazel target build script dependencies.
    pub extra_build_script_bazel_deps: BTreeMap<String, Vec<String>>,
    // Mapping of target triple or spec -> extra bazel target build script data dependencies.
    pub extra_build_script_bazel_data_deps: BTreeMap<String, Vec<String>>,

    pub features_to_remove: BTreeSet<String>,
}

pub struct ConsolidatorConfig {
    // Mapping of crate name to override struct.
    pub overrides: BTreeMap<String, ConsolidatorOverride>,
}

pub struct Consolidator {
    consolidator_config: ConsolidatorConfig,
    render_config: RenderConfig,
    digest: String,
    target_triples: BTreeSet<String>,
    resolved_packages: Vec<CrateContext>,
    member_packages_version_mapping: Dependencies,
    label_to_crates: BTreeMap<String, BTreeSet<String>>,
}

impl Consolidator {
    pub(crate) fn new(
        consolidator_config: ConsolidatorConfig,
        render_config: RenderConfig,
        digest: String,
        target_triples: BTreeSet<String>,
        resolved_packages: Vec<CrateContext>,
        member_packages_version_mapping: Dependencies,
        label_to_crates: BTreeMap<String, BTreeSet<String>>,
    ) -> Self {
        Consolidator {
            consolidator_config,
            render_config,
            digest,
            target_triples,
            resolved_packages,
            member_packages_version_mapping,
            label_to_crates,
        }
    }

    fn targeted_dep_context_for(
        target: &str,
        target_triples_filter: &BTreeSet<String>,
    ) -> CrateTargetedDepContext {
        let platform_targets: Vec<_> = {
            if target.starts_with("cfg(") {
                // User passed a cfg(...) configuration.
                get_matching_bazel_triples(
                    &target.to_owned(),
                    &Some(target_triples_filter.iter().cloned().collect()),
                )
                .map_or_else(|_err| Vec::new(), |i| i.map(|v| v.to_owned()).collect())
            } else {
                // User passed a triple.
                match get_builtin_target_by_triple(target) {
                    Some(_) => vec![target.to_owned()],
                    _ => vec![],
                }
            }
        };
        if platform_targets.is_empty() {
            panic!(
                "Target {} in rule attribute doesn't map to any triple",
                &target
            )
        }

        CrateTargetedDepContext {
            target: target.to_owned(),
            deps: CrateDependencyContext {
                dependencies: BTreeSet::new(),
                proc_macro_dependencies: BTreeSet::new(),
                data_dependencies: BTreeSet::new(),
                build_dependencies: BTreeSet::new(),
                build_proc_macro_dependencies: BTreeSet::new(),
                build_data_dependencies: BTreeSet::new(),
                dev_dependencies: BTreeSet::new(),
                aliased_dependencies: BTreeMap::new(),
            },
            platform_targets,
        }
    }

    // Generate a Vec of CrateTargetedDepContext with the deps/data contents from the provided arguments.
    fn extra_deps_as_targeted_deps<I>(
        target_triples_filter: &BTreeSet<String>,
        extra_bazel_deps: I,
        extra_bazel_data_deps: I,
        extra_build_script_bazel_deps: I,
        extra_build_script_bazel_data_deps: I,
    ) -> Vec<CrateTargetedDepContext>
    where
        I: IntoIterator<Item = (String, Vec<String>)>,
    {
        let mut crate_targeted_dep_contexts: BTreeMap<String, CrateTargetedDepContext> =
            Default::default();

        let into_buildable_dependency = |dep: &String| BuildableDependency {
            name: "".to_string(),
            version: Version::new(0, 0, 0),
            buildable_target: dep.clone(),
            is_proc_macro: false,
        };

        extra_bazel_deps.into_iter().for_each(|(target, deps)| {
            let targeted_dep_context = crate_targeted_dep_contexts
                .entry(target.clone())
                .or_insert_with(|| Self::targeted_dep_context_for(&target, target_triples_filter));
            targeted_dep_context
                .deps
                .dependencies
                .extend(deps.iter().map(into_buildable_dependency));
        });
        extra_bazel_data_deps
            .into_iter()
            .for_each(|(target, deps)| {
                let targeted_dep_context = crate_targeted_dep_contexts
                    .entry(target.clone())
                    .or_insert_with(|| {
                        Self::targeted_dep_context_for(&target, target_triples_filter)
                    });
                targeted_dep_context
                    .deps
                    .data_dependencies
                    .extend(deps.iter().map(into_buildable_dependency));
            });

        extra_build_script_bazel_deps
            .into_iter()
            .for_each(|(target, deps)| {
                let targeted_dep_context = crate_targeted_dep_contexts
                    .entry(target.clone())
                    .or_insert_with(|| {
                        Self::targeted_dep_context_for(&target, target_triples_filter)
                    });
                targeted_dep_context
                    .deps
                    .build_dependencies
                    .extend(deps.iter().map(into_buildable_dependency));
            });

        extra_build_script_bazel_data_deps
            .into_iter()
            .for_each(|(target, deps)| {
                let targeted_dep_context = crate_targeted_dep_contexts
                    .entry(target.clone())
                    .or_insert_with(|| {
                        Self::targeted_dep_context_for(&target, target_triples_filter)
                    });
                targeted_dep_context
                    .deps
                    .build_data_dependencies
                    .extend(deps.iter().map(into_buildable_dependency));
            });

        crate_targeted_dep_contexts
            .into_iter()
            .sorted()
            .map(|(_, targeted_dep_context)| targeted_dep_context)
            .collect()
    }

    pub fn consolidate(self) -> anyhow::Result<Renderer> {
        let Self {
            mut consolidator_config,
            render_config,
            digest,
            target_triples,
            mut resolved_packages,
            member_packages_version_mapping,
            label_to_crates,
        } = self;

        let mut names_and_versions_to_count = BTreeMap::new();
        for pkg in &resolved_packages {
            *names_and_versions_to_count
                .entry((pkg.pkg_name.clone(), pkg.pkg_version.clone()))
                .or_insert(0_usize) += 1_usize;
        }
        let duplicates: Vec<_> = names_and_versions_to_count
            .into_iter()
            .filter_map(|((name, version), value)| {
                if value > 1 {
                    Some(format!("{} {}", name, version))
                } else {
                    None
                }
            })
            .collect();
        if !duplicates.is_empty() {
            return Err(anyhow!(
                "Got duplicate sources for identical crate name and version combination{}: {}",
                if duplicates.len() == 1 { "" } else { "s" },
                duplicates.join(", ")
            ));
        }

        // Apply overrides specified in the crate_universe repo rule.
        for pkg in &mut resolved_packages {
            if let Some(overryde) = consolidator_config.overrides.remove(&pkg.pkg_name) {
                // Add extra dependencies.
                pkg.targeted_deps.extend(Self::extra_deps_as_targeted_deps(
                    &target_triples,
                    overryde.extra_bazel_deps.into_iter(),
                    overryde.extra_bazel_data_deps.into_iter(),
                    overryde.extra_build_script_bazel_deps.into_iter(),
                    overryde.extra_build_script_bazel_data_deps.into_iter(),
                ));
                // Add extra environment variables.
                pkg.raze_settings
                    .additional_env
                    .extend(overryde.extra_rustc_env_vars.into_iter());
                // Add extra build script environment variables.
                pkg.raze_settings
                    .buildrs_additional_environment_variables
                    .extend(overryde.extra_build_script_env_vars.into_iter());

                let features_to_remove = overryde.features_to_remove;
                pkg.features.retain(|f| !features_to_remove.contains(f));
            }
        }

        Ok(Renderer::new(
            render_config,
            digest,
            resolved_packages,
            member_packages_version_mapping,
            label_to_crates,
        ))
    }
}
