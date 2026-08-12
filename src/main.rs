use anyhow::Context as _;
use cargo::{
    core::compiler::{UnitInterner, UserIntent, unit_graph},
    ops::create_bcx,
    util::command_prelude::{ArgMatchesExt, Command, CommandExt as _, ProfileChecking},
};

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

    unit_graph::emit_serialized_unit_graph(&bcx.roots, &bcx.unit_graph, ws.gctx())?;

    Ok(())
}
