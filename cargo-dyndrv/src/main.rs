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
use harmonia_store_path::{StoreDir, StorePath, StorePathName};
use harmonia_store_remote::{DaemonStore, HandshakeDaemonStore as _};
use harmonia_utils_hash::Algorithm::SHA256;
use tokio::{io::BufReader, sync::Mutex};

mod tools;

const SCRIPT_FLAGS_OUTPUT: &str = "flags";
const SCRIPT_IMMEDIATE_ARGS: &str = "args-immediate";
const SCRIPT_TRANSITIVE_ARGS: &str = "args-transitive";
const SCRIPT_IMMEDIATE_ENV: &str = "env";

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

#[derive(Debug, Clone, Default)]
struct UnitCacheMeta {
    pub base_name: Option<String>,
    pub custom_output: bool,
}
#[derive(Debug, Clone)]
struct UnitCache {
    pub drv_path: StorePath,
    pub meta: UnitCacheMeta,
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

fn add_long<T: Display>(args: &mut Vec<bytes::Bytes>, option: &str, value: &T) {
    args.push(format!("--{}={}", option, value).into());
}

fn add_codegen<T: Display>(args: &mut Vec<bytes::Bytes>, option: &str, value: &T) {
    args.push("-C".into());
    args.push(format!("{}={}", option, value).into());
}

async fn add_to_store_nar<T: DaemonStore>(
    store: &mut T,
    real_path: PathBuf,
    name: &str,
) -> eyre::Result<StorePath> {
    static CACHE_MUTEX: Mutex<Option<HashMap<PathBuf, StorePath>>> = Mutex::const_new(None);
    let mut guard = CACHE_MUTEX.lock().await;
    let cache = guard.get_or_insert_default();

    if let Some(store_path) = cache.get(&real_path) {
        Ok(store_path.clone())
    } else {
        // TODO: Don't put the entire nar in memory
        let mut encoder = nix_nar::Encoder::new(&real_path)?;
        let mut buf = Vec::new();
        std::io::copy(&mut encoder, &mut buf)?;
        let reader = BufReader::new(Cursor::new(buf));
        let store_path = store
            .add_ca_to_store(
                name,
                ContentAddressMethodAlgorithm::NixArchive(SHA256),
                &Default::default(),
                false,
                reader,
            )
            .await?
            .path;
        cache.insert(real_path, store_path.clone());
        Ok(store_path)
    }
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

    let tools = tools::Tools::find(&store_dir)?;

    let base_env = tools.base_environment();

    // TODO: find some way of caching this on disk for interactive builds
    let mut drv_cache: Vec<Option<UnitCache>> = vec![None; unit_graph.units.len()];

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

        let src_path = add_to_store_nar(
            &mut store,
            crate_root.to_owned(),
            &format!("{}-src", drv_name),
        )
        .await?;

        let mut env = base_env.clone();

        let mut inputs = BTreeSet::from([
            SingleDerivedPath::Opaque(tools.rustc.store_path.clone()),
            SingleDerivedPath::Opaque(tools.cc.store_path.clone()),
            SingleDerivedPath::Opaque(src_path.clone()),
        ]);

        let (drv, meta) = if unit.mode == CompileMode::RunCustomBuild {
            inputs.insert(SingleDerivedPath::Opaque(
                tools.build_wrap.store_path.clone(),
            ));
            // should have a single dependency, which contains the actual executable
            if unit.dependencies.len() != 1 {
                eyre::bail!("build script has unexpected number of dependencies");
            }
            let dep = drv_cache[unit.dependencies[0].index]
                .as_ref()
                .expect("units out of order");
            inputs.insert(SingleDerivedPath::Built {
                drv_path: Arc::new(SingleDerivedPath::Opaque(dep.drv_path.clone())),
                output: OutputName::default(),
            });
            let mut script = Placeholder::ca_output(&dep.drv_path, &OutputName::default()).render();
            script.push(&unit.dependencies[0].extern_crate_name);

            let flags_dir =
                Placeholder::standard_output(&OutputName::from_str(SCRIPT_FLAGS_OUTPUT).unwrap())
                    .render();
            let out_dir = Placeholder::standard_output(&OutputName::default()).render();

            let args = vec![
                src_path
                    .to_absolute_path(&store_dir)
                    .into_os_string()
                    .into_encoded_bytes()
                    .into(),
                flags_dir.into_os_string().into_encoded_bytes().into(),
                out_dir.into_os_string().into_encoded_bytes().into(),
                script.into_os_string().into_encoded_bytes().into(),
            ];

            (
                Derivation {
                    name: StorePathName::from_str(drv_name).wrap_err("invalid derivation name")?,
                    outputs: BTreeMap::from([
                        (
                            OutputName::default(),
                            DerivationOutput::CAFloating(
                                ContentAddressMethodAlgorithm::NixArchive(SHA256),
                            ),
                        ),
                        (
                            OutputName::from_str(SCRIPT_FLAGS_OUTPUT).unwrap(),
                            DerivationOutput::CAFloating(
                                ContentAddressMethodAlgorithm::NixArchive(SHA256),
                            ),
                        ),
                    ]),
                    inputs,
                    platform: "x86_64-linux".into(),
                    builder: tools
                        .build_wrap
                        .real_path
                        .clone()
                        .into_os_string()
                        .into_encoded_bytes()
                        .into(),
                    args,
                    // TODO: cargo build script environment variables
                    // (https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-build-scripts)
                    env,
                    structured_attrs: None,
                },
                UnitCacheMeta {
                    custom_output: true,
                    ..Default::default()
                },
            )
        } else {
            if unit.target.crate_types.len() != 1 {
                eyre::bail!(
                    "unit {} has unexpected crate types, {:?}",
                    unit.pkg_id,
                    unit.target.crate_types
                );
            }
            let crate_type = &unit.target.crate_types[0];

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
                &Placeholder::standard_output(&OutputName::default())
                    .render()
                    .display(),
            );

            add_long(&mut args, "crate-name", &unit.target.name.replace("-", "_"));
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

            for transitive_dep in &transitive_deps[&unit_idx] {
                let dep_drv = &drv_cache[*transitive_dep].as_ref().unwrap().drv_path;
                inputs.insert(SingleDerivedPath::Built {
                    drv_path: Arc::new(SingleDerivedPath::Opaque(dep_drv.clone())),
                    output: OutputName::default(),
                });

                let placeholder = Placeholder::ca_output(dep_drv, &OutputName::default()).render();
                args.push("-L".into());
                args.push(format!("dependency={}", placeholder.display()).into());
            }

            for direct_dep in &unit.dependencies {
                let dep = drv_cache[direct_dep.index].as_ref().unwrap();
                inputs.insert(SingleDerivedPath::Built {
                    drv_path: Arc::new(SingleDerivedPath::Opaque(dep.drv_path.clone())),
                    output: OutputName::default(),
                });

                if let Some(dep_base_name) = dep.meta.base_name.as_ref() {
                    let placeholder =
                        Placeholder::ca_output(&dep.drv_path, &OutputName::default()).render();
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

            (
                Derivation {
                    name: StorePathName::from_str(drv_name).wrap_err("invalid derivation name")?,
                    outputs: BTreeMap::from([(
                        OutputName::default(),
                        DerivationOutput::CAFloating(ContentAddressMethodAlgorithm::NixArchive(
                            SHA256,
                        )),
                    )]),
                    inputs,
                    platform: "x86_64-linux".into(),
                    builder: tools
                        .rustc
                        .real_path
                        .clone()
                        .into_os_string()
                        .into_encoded_bytes()
                        .into(),
                    args,
                    // TODO: cargo crate environment variables
                    // (https://doc.rust-lang.org/cargo/reference/environment-variables.html)
                    env,
                    structured_attrs: None,
                },
                UnitCacheMeta {
                    base_name,
                    ..Default::default()
                },
            )
        };

        let drv_path = {
            let refs = drv.inputs.iter().map(|p| p.root_path().clone()).collect();
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
        eprintln!("{}", drv_path.to_absolute_path(&store_dir).display());
        drv_cache[unit_idx] = Some(UnitCache { drv_path, meta });
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
