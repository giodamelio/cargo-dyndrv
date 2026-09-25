use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use color_eyre::eyre::{self, Context as _};

#[derive(PartialEq, Eq, Copy, Clone, Debug, Hash, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompileMode {
    Test,
    Build,
    Check,
    Doc,
    Dooctest,
    Docscrape,
    RunCustomBuild,
}

#[derive(Debug, Hash, serde::Deserialize)]
#[allow(unused)]
pub struct Target {
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
    pub name: String,
    pub src_path: PathBuf,
    pub edition: String,
    pub doc: bool,
    pub doctest: bool,
    pub test: bool,
}

#[derive(Debug, Hash, serde::Deserialize)]
#[allow(unused)]
pub struct Profile {
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
pub struct Dependency {
    pub index: usize,
    pub extern_crate_name: String,
}

#[derive(Debug, serde::Deserialize)]
#[allow(unused)]
pub struct Unit {
    pub pkg_id: String,
    pub target: Target,
    pub profile: Profile,
    pub platform: Option<String>,
    pub mode: CompileMode,
    pub features: Vec<String>,
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, serde::Deserialize)]
pub struct UnitGraph {
    pub version: u32,
    pub units: Vec<Unit>,
    pub roots: Vec<usize>,
}

impl UnitGraph {
    pub fn discover(cargo_path: &Path, cargo_args: &[String]) -> eyre::Result<Self> {
        let output = std::process::Command::new(cargo_path)
            .args(cargo_args)
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

        Ok(serde_json::from_slice(&output.stdout)?)
    }
}
