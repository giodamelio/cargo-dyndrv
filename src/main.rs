use std::collections::HashSet;

use anyhow::Context as _;
use cargo::{
    core::{
        SourceKind::Path,
        compiler::{
            Unit, UnitInterner, UserIntent,
            unit_graph::{self, UnitGraph},
        },
    },
    ops::create_bcx,
    util::command_prelude::{ArgMatchesExt, Command, CommandExt as _, ProfileChecking},
};

// It seems like cargo doesn't have an existing function for this?
fn order_units<'a>(
    known_units: &mut HashSet<&Unit>,
    ordered_units: &mut Vec<&'a Unit>,
    unit_graph: &'a UnitGraph,
    root_unit: &Unit,
) {
    let deps = unit_graph
        .get(root_unit)
        .map(|x| x.as_slice())
        .unwrap_or_default();

    for dep in deps {
        if !known_units.contains(&dep.unit) {
            ordered_units.push(&dep.unit);
        }
    }
}

fn main() -> anyhow::Result<()> {
    // rustc things.
    // Could be replaced with a `cargo build --unit-graph`
    // by either using nightly cargo or waiting until it is stabilised:
    // https://github.com/rust-lang/cargo/issues/8002

    let args = Command::new("cargo-dyndrv")
        .about("Check a local package and all of its dependencies for errors")
        .arg_future_incompat_report()
        .arg_message_format()
        .arg_silent_suggestion()
        .arg_package_spec(
            "Package to build (see `cargo help pkgid`)",
            "Build all packages in the workspace",
            "Exclude packages from the build",
        )
        .arg_targets_all(
            "Build only this package's library",
            "Build only the specified binary",
            "Build all binaries",
            "Build only the specified example",
            "Build all examples",
            "Build only the specified test target",
            "Build all targets that have `test = true` set",
            "Build only the specified bench target",
            "Build all targets that have `bench = true` set",
            "Build all targets",
        )
        .arg_features()
        .arg_release("Build artifacts in release mode, with optimizations")
        .arg_redundant_default_mode("debug", "build", "release")
        .arg_profile("Build artifacts with the specified profile")
        .arg_parallel()
        .arg_target_triple("Build for the target triple")
        .arg_target_dir()
        .arg_artifact_dir()
        .arg_timings()
        .arg_compile_time_deps()
        .arg_manifest_path()
        .arg_ignore_rust_version()
        .get_matches();

    let gctx = cargo::util::GlobalContext::default().context("creating Cargo global context")?;

    let ws = args
        .workspace(&gctx)
        .context("creating Cargo workspace object")?;

    let intent = match args.get_one::<String>("profile").map(String::as_str) {
        Some("test") => UserIntent::Test,
        Some("bench") => UserIntent::Bench,
        Some("check") => UserIntent::Check { test: false },
        _ => UserIntent::Build,
    };

    let compile_opts = args
        .compile_options(&gctx, intent, Some(&ws), ProfileChecking::Custom)
        .context("creating compile options")?;
    let interner = UnitInterner::new();

    let bcx = create_bcx(&ws, &compile_opts, &interner, None).context("resolving workspace")?;

    // TODO: improve sorting
    let units = {
        let mut known_units = HashSet::with_capacity(bcx.unit_graph.len());
        let mut units = Vec::with_capacity(bcx.unit_graph.len());
        for root in &bcx.roots {
            order_units(&mut known_units, &mut units, &bcx.unit_graph, root);
        }
        units
    };

    // TODO: persist caches.
    // May be a good idea to make units for Cargo.lock and persist their outputs
    // in a registry cache
    for unit in units {}

    Ok(())
}
