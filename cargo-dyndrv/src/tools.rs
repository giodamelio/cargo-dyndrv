use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use bytes::Bytes;
use color_eyre::eyre::{self, Context as _};
use harmonia_store_path::{StoreDir, StorePath};

use crate::util::{CloneBytes, IntoBytes};

fn containing_store_path(store_dir: &StoreDir, path: &Path) -> Option<StorePath> {
    let mut components = path.strip_prefix(store_dir.to_path()).ok()?.components();

    let std::path::Component::Normal(part) = components.next()? else {
        return None;
    };
    let store_path = StorePath::from_base_path(part.to_str()?).ok()?;
    Some(store_path)
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub store_path: StorePath,
    pub real_path: PathBuf,
}

impl Tool {
    fn find(store_dir: &StoreDir, tool_name: &str, fallback: Option<&str>) -> eyre::Result<Self> {
        let var_name = tool_name.to_uppercase().replace('-', "_");
        let real_path = if let Some(discovered) = std::env::var_os(var_name) {
            which::which(&discovered).wrap_err_with(|| {
                format!(
                    "could not find tool {}, tried {}",
                    tool_name,
                    discovered.display()
                )
            })?
        } else if let Some(fallback) = fallback {
            fallback.into()
        } else {
            which::which(tool_name)
                .wrap_err_with(|| format!("could not find tool {}", tool_name))?
        };

        let store_path = containing_store_path(store_dir, &real_path)
            .ok_or_else(|| eyre::eyre!("tool {} is not in the Nix store", tool_name))?;

        Ok(Self {
            store_path,
            real_path,
        })
    }
}

pub struct Tools {
    pub rustc: Tool,
    pub cargo: Tool,
    pub target_cc: Tool,
    pub host_cc: Tool,
    pub target_cxx: Tool,
    pub host_cxx: Tool,
    pub env_wrap: Tool,
    pub build_wrap: Tool,
    pub target_env: Tool,
    pub ln: Tool,
}

impl Tools {
    pub fn find(store_dir: &StoreDir) -> eyre::Result<Self> {
        let cc = Tool::find(store_dir, "cc", None)?;
        // This is really CC_FOR_BUILD, but this is what Rust calls it
        let host_cc = Tool::find(store_dir, "host-cc", None).unwrap_or_else(|_| cc.clone());
        let cxx = Tool::find(store_dir, "cxx", None)?;
        // This is really CC_FOR_BUILD, but this is what Rust calls it
        let host_cxx = Tool::find(store_dir, "host-cxx", None).unwrap_or_else(|_| cc.clone());

        Ok(Self {
            rustc: Tool::find(store_dir, "rustc", None)?,
            cargo: Tool::find(store_dir, "cargo", None)?,
            target_cc: cc,
            host_cc,
            target_cxx: cxx,
            host_cxx,
            env_wrap: Tool::find(store_dir, "env-wrap", option_env!("ENV_WRAP"))?,
            build_wrap: Tool::find(store_dir, "build-wrap", option_env!("BUILD_WRAP"))?,
            target_env: Tool::find(store_dir, "target-env", option_env!("TARGET_ENV"))?,
            ln: Tool::find(store_dir, "ln", option_env!("LN"))?,
        })
    }

    pub fn base_environment(&self) -> BTreeMap<Bytes, Bytes> {
        // technially rustc doesn't need to be in path,
        // they could just be variables and codegen args,
        // but build.rs scripts make assumptions
        let path = self.rustc.real_path.parent().unwrap().to_owned();

        BTreeMap::from([
            ("PATH".into(), path.into_bytes()),
            ("RUSTC".into(), self.rustc.real_path.clone_bytes()),
        ])
    }
}
