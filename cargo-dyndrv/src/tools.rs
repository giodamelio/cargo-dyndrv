use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use bytes::Bytes;
use color_eyre::eyre::{self, Context as _};
use harmonia_store_path::{StoreDir, StorePath};

fn containing_store_path(store_dir: &StoreDir, path: &Path) -> Option<StorePath> {
    let mut components = path.strip_prefix(store_dir.to_path()).ok()?.components();

    let std::path::Component::Normal(part) = components.next()? else {
        return None;
    };
    let store_path = StorePath::from_base_path(part.to_str()?).ok()?;
    Some(store_path)
}

pub struct Tool {
    pub store_path: StorePath,
    pub real_path: PathBuf,
}

impl Tool {
    fn find(store_dir: &StoreDir, tool_name: &str) -> eyre::Result<Self> {
        let var_name = tool_name.to_uppercase().replace('-', "_");
        let real_path = if let Some(discovered) = std::env::var_os(var_name) {
            which::which(&discovered).wrap_err_with(|| {
                format!(
                    "could not find tool {}, tried {}",
                    tool_name,
                    discovered.display()
                )
            })?
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
    pub cc: Tool,
    pub env_wrap: Tool,
    pub build_wrap: Tool,
}

impl Tools {
    pub fn find(store_dir: &StoreDir) -> eyre::Result<Self> {
        Ok(Self {
            rustc: Tool::find(store_dir, "rustc")?,
            cc: Tool::find(store_dir, "cc")?,
            env_wrap: Tool::find(store_dir, "env-wrap")?,
            build_wrap: Tool::find(store_dir, "build-wrap")?,
        })
    }

    pub fn base_environment(&self) -> BTreeMap<Bytes, Bytes> {
        // technially they don't need to be in path,
        // they could just be variables,
        // but some build.rs scripts could make bad assumptions.
        let path = format!(
            "{}:{}",
            self.rustc.real_path.parent().unwrap().display(),
            self.cc.real_path.parent().unwrap().display()
        );

        BTreeMap::from([
            ("PATH".into(), path.into()),
            (
                "CC".into(),
                self.cc
                    .real_path
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec()
                    .into(),
            ),
            (
                "RUSTC".into(),
                self.rustc
                    .real_path
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec()
                    .into(),
            ),
        ])
    }
}
