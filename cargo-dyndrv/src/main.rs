use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
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
use harmonia_store_path::{FromStoreDirStr, StoreDir, StorePath, StorePathName};
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

#[derive(Debug, serde::Deserialize)]
struct ExternConfig {
    #[serde(default)]
    pub extra_deps: Vec<String>,
    #[serde(default)]
    pub extra_env: BTreeMap<String, String>,
    #[serde(default)]
    pub extra_path: Vec<String>,
}

fn order_units(
    all_transitive_deps: &mut HashMap<usize, BTreeSet<usize>>,
    ordered_units: &mut Vec<usize>,
    all_units: &[Unit],
    idx: usize,
) {
    if all_transitive_deps.contains_key(&idx) {
        return;
    }

    let mut transitive_deps = BTreeSet::new();
    let unit = &all_units[idx];
    for dep in &unit.dependencies {
        order_units(all_transitive_deps, ordered_units, all_units, dep.index);
        transitive_deps.append(&mut all_transitive_deps[&dep.index].clone());
        transitive_deps.insert(dep.index);
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

fn add_long<T: Display>(args: &mut VecDeque<bytes::Bytes>, option: &str, value: &T) {
    args.push_back(format!("--{}={}", option, value).into());
}

fn add_codegen<T: Display>(args: &mut VecDeque<bytes::Bytes>, option: &str, value: &T) {
    args.push_back("-C".into());
    args.push_back(format!("{}={}", option, value).into());
}

fn add_feature(args: &mut VecDeque<bytes::Bytes>, feature: &str) {
    args.push_back("--cfg".into());
    args.push_back(format!("feature=\"{}\"", feature).into());
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

    // TODO: accept this via some argument

    let all_extern_config: BTreeMap<String, ExternConfig> =
        if let Ok(content) = std::fs::read_to_string("extern.json") {
            serde_json::from_str(&content).context("Could not parse extern configuration")?
        } else {
            Default::default()
        };

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
        // TODO: more cargo env vars, perhaps via CARGO_METADATA
        env.insert("TARGET".into(), "x86_64-unknown-linux-gnu".into());
        if let Some((_, version_str)) = unit.pkg_id.rsplit_once('@') {
            let version = semver::Version::parse(version_str).wrap_err("parsing version")?;
            env.insert("CARGO_PKG_VERSION".into(), version_str.to_owned().into());
            env.insert(
                "CARGO_PKG_VERSION_MAJOR".into(),
                version.major.to_string().into(),
            );
            env.insert(
                "CARGO_PKG_VERSION_MINOR".into(),
                version.minor.to_string().into(),
            );
            env.insert(
                "CARGO_PKG_VERSION_PATCH".into(),
                version.patch.to_string().into(),
            );
            env.insert(
                "CARGO_PKG_VERSION_PRE".into(),
                version.pre.as_str().to_owned().into(),
            );
        }

        let mut inputs = BTreeSet::from([
            SingleDerivedPath::Opaque(tools.rustc.store_path.clone()),
            SingleDerivedPath::Opaque(tools.cc.store_path.clone()),
            SingleDerivedPath::Opaque(src_path.clone()),
        ]);

        let (drv, meta) = if unit.mode == CompileMode::RunCustomBuild {
            inputs.insert(SingleDerivedPath::Opaque(
                tools.build_wrap.store_path.clone(),
            ));

            let mut executable_dep = None;
            // cargo gives us a few dependencies.
            // one has the actual built executable, the others are build script
            // executions of dependencies that set metadata
            for dep in &unit.dependencies {
                let dep_unit = &unit_graph.units[dep.index];
                if dep_unit.mode == CompileMode::Build {
                    executable_dep = Some(dep);
                } else if dep_unit.mode == CompileMode::RunCustomBuild {
                    // TODO: whatever the hell i'm supposed to do here
                }
            }
            let Some(executable_dep) = executable_dep else {
                eyre::bail!("build script execution did not specify what to run");
            };

            let executable_cache = drv_cache[executable_dep.index]
                .as_ref()
                .expect("units out of order");
            inputs.insert(SingleDerivedPath::Built {
                drv_path: Arc::new(SingleDerivedPath::Opaque(executable_cache.drv_path.clone())),
                output: OutputName::default(),
            });
            let mut script =
                Placeholder::ca_output(&executable_cache.drv_path, &OutputName::default()).render();
            script.push(&executable_dep.extern_crate_name);

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

            if let Some(extern_config) = all_extern_config.get(&unit.pkg_id) {
                eprintln!("Handling external config for {}", unit.pkg_id);
                for extra_dep in &extern_config.extra_deps {
                    let store_path = StorePath::from_store_dir_str(&store_dir, extra_dep)
                        .wrap_err("Invalid store path in extra")?;
                    inputs.insert(SingleDerivedPath::Opaque(store_path));
                }

                for (var, value) in &extern_config.extra_env {
                    env.insert(var.clone().into(), value.clone().into());
                }

                let mut path: bytes::BytesMut =
                    env.remove(&bytes::Bytes::from("PATH")).unwrap().into();
                for item in &extern_config.extra_path {
                    path.extend_from_slice(b":");
                    path.extend_from_slice(item.as_bytes());
                }
                env.insert("PATH".into(), path.into());
            }

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

            // nix derivation args start at argv[1], but we put rustc in here anyway.
            // It's easier to add the wrapper if needed
            let mut args = VecDeque::from([tools
                .rustc
                .real_path
                .clone()
                .into_os_string()
                .into_encoded_bytes()
                .into()]);
            {
                // rustc handles finding other source files for us
                let crate_relative = unit
                    .target
                    .src_path
                    .strip_prefix(crate_root)
                    .wrap_err("internal: crate main is not in crate root???")?;
                let mut path = src_path.to_absolute_path(&store_dir);
                path.push(crate_relative);
                args.push_back(path.into_os_string().into_encoded_bytes().into())
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

            for feature in &unit.features {
                add_feature(&mut args, feature);
            }

            for transitive_dep in &transitive_deps[&unit_idx] {
                let dep = drv_cache[*transitive_dep].as_ref().unwrap();
                inputs.insert(SingleDerivedPath::Built {
                    drv_path: Arc::new(SingleDerivedPath::Opaque(dep.drv_path.clone())),
                    output: OutputName::default(),
                });

                let placeholder =
                    Placeholder::ca_output(&dep.drv_path, &OutputName::default()).render();
                args.push_back("-L".into());
                args.push_back(format!("dependency={}", placeholder.display()).into());

                if dep.meta.custom_output {
                    inputs.insert(SingleDerivedPath::Built {
                        drv_path: Arc::new(SingleDerivedPath::Opaque(dep.drv_path.clone())),
                        output: OutputName::from_str(SCRIPT_FLAGS_OUTPUT).unwrap(),
                    });

                    let flags = Placeholder::ca_output(
                        &dep.drv_path,
                        &OutputName::from_str(SCRIPT_FLAGS_OUTPUT).unwrap(),
                    )
                    .render();

                    args.push_back(
                        format!("@{}", flags.join(SCRIPT_TRANSITIVE_ARGS).display()).into(),
                    );
                }
            }

            for direct_dep in &unit.dependencies {
                // no need to add inputs here, they're already handled from transitive deps
                let dep = drv_cache[direct_dep.index].as_ref().unwrap();

                let out = Placeholder::ca_output(&dep.drv_path, &OutputName::default()).render();

                if let Some(dep_base_name) = dep.meta.base_name.as_ref() {
                    args.push_back("--extern".into());
                    args.push_back(
                        format!(
                            "{}={}/{}.{}",
                            direct_dep.extern_crate_name,
                            out.display(),
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
                if dep.meta.custom_output {
                    inputs.insert(SingleDerivedPath::Opaque(tools.env_wrap.store_path.clone()));
                    env.insert(
                        "OUT_DIR".into(),
                        out.into_os_string().into_encoded_bytes().into(),
                    );
                    let flags = Placeholder::ca_output(
                        &dep.drv_path,
                        &OutputName::from_str(SCRIPT_FLAGS_OUTPUT).unwrap(),
                    )
                    .render();

                    // reversed since we're pushing from the front
                    args.push_front("--".into());
                    args.push_front(
                        flags
                            .join(SCRIPT_IMMEDIATE_ENV)
                            .into_os_string()
                            .into_encoded_bytes()
                            .into(),
                    );
                    args.push_front(
                        tools
                            .env_wrap
                            .real_path
                            .as_os_str()
                            .as_encoded_bytes()
                            .to_owned()
                            .into(),
                    );

                    args.push_back(
                        format!("@{}", flags.join(SCRIPT_IMMEDIATE_ARGS).display()).into(),
                    );
                }
            }

            let builder = args.pop_front().unwrap();
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
                    builder,
                    args: args.into(),
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
