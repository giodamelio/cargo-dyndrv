use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsStr,
    fmt::Display,
    io::Cursor,
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    sync::Arc,
};

use color_eyre::eyre::{self, OptionExt as _, WrapErr as _};
use harmonia_store_content_address::ContentAddressMethodAlgorithm;
use harmonia_store_derivation::{
    derivation::{Derivation, DerivationOutput},
    derived_path::{OutputName, SingleDerivedPath},
    placeholder::Placeholder,
};
use harmonia_store_path::{StoreDir, StorePath, StorePathName, StorePathSet};
use harmonia_store_remote::{DaemonStore, HandshakeDaemonStore as _};
use harmonia_utils_hash::Algorithm::SHA256;
use tokio::io::BufReader;

#[derive(PartialEq, Eq, Copy, Clone, Debug, serde::Deserialize)]
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
#[allow(unused)]
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
#[allow(unused)]
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
#[allow(unused)]
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

#[derive(Debug, Clone)]
struct UnitCache {
    pub drv_path: StorePath,
    pub base_name: Option<String>,
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

fn containing_store_path(store_dir: &StoreDir, path: &Path) -> Option<StorePath> {
    let mut components = path.strip_prefix(store_dir.to_path()).ok()?.components();

    let std::path::Component::Normal(part) = components.next()? else {
        return None;
    };
    let store_path = StorePath::from_base_path(part.to_str()?).ok()?;
    Some(store_path)
}

fn find_tool(store_dir: &StoreDir, tool_name: &str) -> eyre::Result<(PathBuf, StorePath)> {
    let tool_path = if let Some(discovered) = std::env::var_os(tool_name.to_uppercase()) {
        which::which(&discovered).wrap_err_with(|| {
            format!(
                "could not find tool {}, tried {}",
                tool_name,
                discovered.display()
            )
        })?
    } else {
        which::which(tool_name).wrap_err_with(|| format!("could not find tool {}", tool_name))?
    };

    let store_path = containing_store_path(store_dir, &tool_path)
        .ok_or_else(|| eyre::eyre!("tool {} is not in the Nix store", tool_name))?;

    Ok((tool_path, store_path))
}

fn add_long<T: Display>(args: &mut Vec<bytes::Bytes>, option: &str, value: &T) {
    args.push(format!("--{}={}", option, value).into());
}

fn add_codegen<T: Display>(args: &mut Vec<bytes::Bytes>, option: &str, value: &T) {
    args.push("-C".into());
    args.push(format!("{}={}", option, value).into());
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
        for root in &unit_graph.roots {
            order_units(&mut transitive_deps, &mut units, &unit_graph.units, *root);
        }
        (units, transitive_deps)
    };

    // Someone might want a different one for some reason, idk how to get it
    let store_dir = StoreDir::default();
    let mut store = harmonia_store_remote::DaemonClientBuilder::new()
        .set_store_dir(&store_dir)
        .build_daemon()
        .await?
        .handshake()
        .await?;

    let (rustc_path, rustc_store_path) = find_tool(&store_dir, "rustc")?;
    let (cc_path, cc_store_path) = find_tool(&store_dir, "cc")?;

    let mut base_env = BTreeMap::new();
    base_env.insert(
        "PATH".into(),
        format!(
            "{}:{}",
            rustc_path.parent().unwrap().display(),
            cc_path.parent().unwrap().display()
        )
        .into(),
    );

    // TODO: find some way of caching this on disk for interactive builds
    let mut src_paths = HashMap::<&Path, StorePath>::new();
    let mut drv_cache: Vec<Option<UnitCache>> = vec![None; unit_graph.units.len()];

    let output_out = OutputName::from_str("out").unwrap();

    for unit_idx in ordered_units {
        // TODO: wrap rustc so we can get additional args from
        // build.rs outputs
        let unit = &unit_graph.units[unit_idx];

        if unit.target.crate_types.len() != 1 {
            eyre::bail!(
                "unit {} has unexpected crate types, {:?}",
                unit.pkg_id,
                unit.target.crate_types
            );
        }
        let crate_type = &unit.target.crate_types[0];

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

        let mut args = Vec::new();
        // nix derivation args start at argv[1], no `rustc` here
        {
            // rustc handles finding other source files for us
            let crate_relative = unit
                .target
                .src_path
                .strip_prefix(crate_root)
                .wrap_err("internal: crate main is not in crate root???")?;
            let mut path = src_path.to_absolute_path(&store_dir);
            path.push(crate_relative);
            args.push(path.into_os_string().into_encoded_bytes().into())
        }

        add_long(
            &mut args,
            "out-dir",
            &Placeholder::standard_output(&output_out).render().display(),
        );

        add_long(&mut args, "crate-name", &unit.target.name);
        add_long(&mut args, "edition", &unit.target.edition);
        add_long(&mut args, "crate-type", crate_type);

        if unit.mode == CompileMode::Check {
            add_long(&mut args, "emit", &"metadata");
        } else if unit.mode == CompileMode::Build {
            if crate_type == "lib" || crate_type == "rlib" {
                add_long(&mut args, "emit", &"metadata,link");
            } else {
                add_long(&mut args, "emit", &"link");
            }
        }

        add_codegen(&mut args, "debuginfo", &unit.profile.debuginfo);

        // TODO: embed-bitcode, lto
        // TODO: check-cfg
        // TODO: for real this time, it's really necessary metadata and extra filename
        add_codegen(&mut args, "metadata", &format_args!("{:016x}", u64::MAX));
        let base_name = if unit.target.crate_types.contains(&String::from("lib")) {
            let extra = format!("-{:016x}", u64::MAX);
            add_codegen(&mut args, "extra-filename", &extra);
            // cargo uses rustc outputs to learn rmeta locations.
            // we don't have that luxury, but the default path is documented.
            Some(format!("lib{}{}", unit.target.name, extra))
        } else {
            None
        };

        let mut refs = StorePathSet::new();
        refs.insert(rustc_store_path.clone());
        refs.insert(cc_store_path.clone());

        let mut inputs = BTreeSet::<SingleDerivedPath>::new();
        inputs.insert(SingleDerivedPath::Opaque(rustc_store_path.clone()));
        inputs.insert(SingleDerivedPath::Opaque(cc_store_path.clone()));
        inputs.insert(SingleDerivedPath::Opaque(src_path.clone()));

        for transitive_dep in &transitive_deps[&unit_idx] {
            let dep_drv = &drv_cache[*transitive_dep].as_ref().unwrap().drv_path;
            refs.insert(dep_drv.clone());
            inputs.insert(SingleDerivedPath::Built {
                drv_path: Arc::new(SingleDerivedPath::Opaque(dep_drv.clone())),
                output: output_out.clone(),
            });

            let placeholder = Placeholder::ca_output(dep_drv, &output_out).render();
            args.push("-L".into());
            args.push(format!("dependency={}", placeholder.display()).into());
        }

        for direct_dep in &unit.dependencies {
            let dep = drv_cache[direct_dep.index].as_ref().unwrap();
            refs.insert(dep.drv_path.clone());
            inputs.insert(SingleDerivedPath::Built {
                drv_path: Arc::new(SingleDerivedPath::Opaque(dep.drv_path.clone())),
                output: output_out.clone(),
            });

            if let Some(dep_base_name) = dep.base_name.as_ref() {
                let placeholder = Placeholder::ca_output(&dep.drv_path, &output_out).render();
                args.push("--extern".into());
                args.push(
                    format!(
                        "{}={}/{}.{}",
                        direct_dep.extern_crate_name,
                        placeholder.display(),
                        dep_base_name,
                        if crate_type == "lib" || crate_type == "rlib" {
                            "rmeta"
                        } else {
                            "rlib"
                        },
                    )
                    .into(),
                );
            }
        }

        let drv = Derivation {
            name: StorePathName::from_str(drv_name).wrap_err("invalid derivation name")?,
            outputs: BTreeMap::from([(
                output_out.clone(),
                DerivationOutput::CAFloating(ContentAddressMethodAlgorithm::NixArchive(SHA256)),
            )]),
            inputs,
            platform: "x86_64-linux".into(),
            builder: rustc_path
                .clone()
                .into_os_string()
                .into_encoded_bytes()
                .into(),
            args,
            env: base_env.clone(),
            structured_attrs: None,
        };

        let drv_path = {
            let bytes = harmonia_store_aterm::print_derivation_aterm(&store_dir, &drv.into_full());
            let source = BufReader::new(Cursor::new(bytes.clone()));
            store
                .add_ca_to_store(
                    &format!("{}.drv", drv_name),
                    ContentAddressMethodAlgorithm::Text,
                    &refs,
                    false,
                    source,
                )
                .await?
                .path
        };
        drv_cache[unit_idx] = Some(UnitCache {
            drv_path,
            base_name,
        });
    }

    for unit_idx in unit_graph.roots {
        println!(
            "{}",
            store_dir.display(&drv_cache[unit_idx].as_ref().unwrap().drv_path)
        );
    }

    // TODO: run the build if we are outside a derivation,
    // attach to output if we are inside a derivation.

    Ok(())
}
