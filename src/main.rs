#![allow(unused)]

use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fmt::Display,
    io::Cursor,
    path::{Path, PathBuf},
    process::Stdio,
};

use color_eyre::eyre::{self, OptionExt as _, WrapErr as _};
use harmonia_store_content_address::ContentAddressMethodAlgorithm;
use harmonia_store_path::{StoreDir, StorePath};
use harmonia_store_remote::{DaemonStore, HandshakeDaemonStore as _};
use harmonia_utils_hash::Algorithm::SHA256;
use tokio::io::BufReader;

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
enum CompileMode {
    Test,
    Build,
    Check,
    Doc,
    Dooctest,
    Docscrape,
    RunCustomBuild,
}

#[derive(Debug, serde::Deserialize)]
struct Target {
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
    pub name: String,
    pub src_path: PathBuf,
    pub edition: String,
    pub doc: bool,
    pub doctest: bool,
    pub test: bool,
}

#[derive(Debug, serde::Deserialize)]
struct Profile {
    pub name: String,
    pub opt_level: String,
    pub lto: String,
    pub codegen_backend: Option<String>,
    pub codgen_units: Option<u32>,
    pub debuginfo: u32,
    pub split_debuginfo: Option<String>,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub rpath: bool,
    pub incremental: bool,
    pub panic: String,
    // TODO: Strip
}

#[derive(Debug, serde::Deserialize)]
struct Dependency {
    pub index: usize,
    pub extern_crate_name: String,
}

#[derive(Debug, serde::Deserialize)]
struct Unit {
    pub pkg_id: String,
    pub target: Target,
    pub profile: Profile,
    pub platform: Option<String>,
    pub mode: CompileMode,
    pub features: Vec<String>,
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, serde::Deserialize)]
struct UnitGraph {
    pub version: u32,
    pub units: Vec<Unit>,
    pub roots: Vec<usize>,
}

fn order_units(
    all_transitive_deps: &mut HashMap<usize, Vec<usize>>,
    ordered_units: &mut Vec<usize>,
    all_units: &[Unit],
    idx: usize,
) {
    if all_transitive_deps.contains_key(&idx) {
        return;
    }

    let mut transitive_deps = Vec::new();
    let unit = &all_units[idx];
    for dep in &unit.dependencies {
        order_units(all_transitive_deps, ordered_units, all_units, dep.index);
        transitive_deps.extend_from_slice(&all_transitive_deps[&dep.index]);
    }

    all_transitive_deps.insert(idx, transitive_deps);

    ordered_units.push(idx);
}

/// HACK: TODO: really need a better way of getting this
fn find_crate_root(src_path: &Path) -> Option<&Path> {
    // technically rustc only needs the immediate parent,
    // but many crates want CARGO_MANIFEST_DIR
    let parent = src_path.parent();
    if parent.and_then(Path::file_name) == Some(OsStr::new("src")) {
        parent.and_then(Path::parent)
    } else {
        parent
    }
}

fn containing_store_path<'a>(
    store_dir: &StoreDir,
    path: &'a Path,
) -> Option<(StorePath, &'a Path)> {
    let mut components = path.strip_prefix(store_dir.to_path()).ok()?.components();

    let std::path::Component::Normal(part) = components.next()? else {
        return None;
    };
    let store_path = StorePath::from_base_path(part.to_str()?).ok()?;
    Some((store_path, components.as_path()))
}

fn add_long<T: Display>(rustc_args: &mut Vec<String>, option: &str, value: &T) {
    rustc_args.push(format!("--{}={}", option, value));
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    // Shelling out to `cargo` since the cargo crate does not provide what we need

    let unit_graph: UnitGraph = {
        let sys_args: Vec<_> = std::env::args_os().collect();
        let output = std::process::Command::new("cargo")
            .args(&sys_args[1..])
            .arg("-Z")
            .arg("unstable-options")
            .arg("--unit-graph")
            .stderr(Stdio::inherit())
            .output()
            .wrap_err("Executing cargo")?;

        if !output.status.success() {
            eyre::bail!(
                "Cargo failed with error {}",
                output.status.code().unwrap_or(-1)
            )
        }

        serde_json::from_slice(&output.stdout)?
    };

    if unit_graph.version != 1 {
        eyre::bail!("Unsupported unit graph version {}", unit_graph.version);
    }

    // TODO: parse Cargo.toml and make registry to cache packages in store.
    // Would be useful to map package IDs to store paths persistently
    // TODO: improve sorting
    let (ordered_units, transitive_deps) = {
        let mut transitive_deps = HashMap::with_capacity(unit_graph.units.len());
        let mut units = Vec::with_capacity(unit_graph.units.len());
        for root in unit_graph.roots {
            order_units(&mut transitive_deps, &mut units, &unit_graph.units, root);
        }
        (units, transitive_deps)
    };

    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(Into::into)
        .or_else(std::env::home_dir)
        .ok_or_eyre("Could not find home directory")?;

    let rustc_path = if let Some(path) = std::env::var_os("RUSTC").map(Into::into) {
        path
    } else {
        which::which("rustc").wrap_err("Could not find rustc")?
    };

    // Someone might want a different one for some reason, idk how to get it
    let store_dir = StoreDir::default();
    let mut store = harmonia_store_remote::DaemonClientBuilder::new()
        .set_store_dir(&store_dir)
        .build_daemon()
        .await?
        .handshake()
        .await?;

    let (rustc_store_path, _) =
        containing_store_path(&store_dir, &rustc_path).ok_or_eyre("rustc was not in Nix store")?;

    // TODO: find some way of caching this on disk for interactive builds
    let mut src_paths = HashMap::<&Path, StorePath>::new();
    let mut drv_paths = HashMap::<usize, StorePath>::new();
    for unit_idx in ordered_units {
        // TODO: wrap rustc so we can get additional args from
        // build.rs outputs
        let unit = &unit_graph.units[unit_idx];

        let crate_root =
            find_crate_root(&unit.target.src_path).ok_or_eyre("unit does not have source path")?;

        let drv_name = crate_root
            .file_name()
            .ok_or_eyre("empty path")?
            .to_str()
            .ok_or_eyre("invalid src path name")?;

        if !src_paths.contains_key(crate_root) {
            // TODO: Don't put the entire nar in memory
            let mut encoder = nix_nar::Encoder::new(crate_root)?;
            let mut buf = Vec::new();
            std::io::copy(&mut encoder, &mut buf)?;
            let reader = BufReader::new(Cursor::new(buf));
            let path = store
                .add_ca_to_store(
                    &format!("{}-src", drv_name),
                    ContentAddressMethodAlgorithm::NixArchive(SHA256),
                    &Default::default(),
                    false,
                    reader,
                )
                .await?
                .path;
            src_paths.insert(crate_root, path);
        }

        let src_path = &src_paths[crate_root];

        println!(
            "{} -> {}",
            crate_root.display(),
            store_dir.display(src_path)
        );

        let mut rustc_args = Vec::new();
        // nix derivation args start at argv[1], no `rustc` here
        add_long(&mut rustc_args, "crate-name", &unit.target.name);

        add_long(&mut rustc_args, "edition", &unit.target.edition);

        {
            let crate_relative = unit
                .target
                .src_path
                .strip_prefix(crate_root)
                .wrap_err("internal: crate main is not in crate root???")?;
            let mut path = src_path.to_absolute_path(&store_dir);
            path.push(crate_relative);
            rustc_args.push(
                path.into_os_string()
                    .into_string()
                    .ok()
                    .ok_or_eyre("path not valid utf-8")?,
            )
        }

        add_long(
            &mut rustc_args,
            "--crate-type",
            &unit.target.crate_types.join(","),
        );
    }

    // TODO: run the build if we are outside a derivation,
    // attach to output if we are inside a derivation.

    Ok(())
}
