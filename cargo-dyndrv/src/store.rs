use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    str::FromStr,
    sync::{Arc, OnceLock},
};

use color_eyre::eyre::{self, Context};
use harmonia_store_content_address::ContentAddressMethodAlgorithm;
use harmonia_store_derivation::{
    derivation::{Derivation, DerivationOutput},
    derived_path::{OutputName, SingleDerivedPath},
    placeholder::Placeholder,
};
use harmonia_store_path::{StoreDir, StorePath, StorePathName};
use harmonia_store_remote::DaemonStore;
use harmonia_utils_hash::Algorithm::SHA256;
use tokio::sync::Mutex;

use crate::{
    tools::Tool,
    util::{CloneBytes, IntoBytes},
};

pub fn is_in_derivation() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| std::env::var_os("NIX_BUILD_TOP").is_some())
}

pub fn daemon_path() -> PathBuf {
    if let Ok(from_env) = std::env::var("NIX_REMOTE") {
        from_env
            .strip_prefix("unix://")
            .map(ToString::to_string)
            .unwrap_or(from_env)
            .into()
    } else {
        "/nix/var/nix/daemon-socket/socket".into()
    }
}

pub async fn add_to_store_nar<T: DaemonStore>(
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
        let store_path = store
            .add_ca_to_store(
                name,
                ContentAddressMethodAlgorithm::NixArchive(SHA256),
                &Default::default(),
                false,
                buf.as_slice(),
            )
            .await?
            .path;
        cache.insert(real_path, store_path.clone());
        Ok(store_path)
    }
}

pub async fn add_drv_to_store<T: DaemonStore>(
    store: &mut T,
    store_dir: &StoreDir,
    drv: Derivation,
    drv_name: &str,
) -> eyre::Result<StorePath> {
    let refs = drv.inputs.iter().map(|p| p.root_path().clone()).collect();
    let buf = harmonia_store_aterm::print_derivation_aterm(store_dir, &drv.into_full());
    Ok(store
        .add_ca_to_store(
            &format!("{}.drv", drv_name),
            ContentAddressMethodAlgorithm::Text,
            &refs,
            false,
            buf.as_slice(),
        )
        .await?
        .path)
}

pub async fn submit_wrappers<T: DaemonStore>(
    store: &mut T,
    store_dir: &StoreDir,
    ln: &Tool,
    outputs: &BTreeMap<&str, &StorePath>,
) -> eyre::Result<()> {
    // We're only allowed to submit if the name is correct.
    // The easiest way to do this without breaking the inner loop is a symlink
    let wrapper_drv = Derivation {
        name: StorePathName::from_str("cargo-dyndrv-build.drv")?,
        outputs: outputs
            .keys()
            .map(|name| {
                Ok((
                    OutputName::from_str(name).wrap_err("crate name is not valid output")?,
                    DerivationOutput::CAFloating(ContentAddressMethodAlgorithm::NixArchive(SHA256)),
                ))
            })
            .collect::<eyre::Result<_>>()?,
        inputs: outputs
            .values()
            .map(|drv_path| SingleDerivedPath::Built {
                drv_path: Arc::new(SingleDerivedPath::Opaque((*drv_path).clone())),
                output: OutputName::default(),
            })
            .chain([SingleDerivedPath::Opaque(ln.store_path.clone())])
            .collect(),
        platform: "x86_64-linux".into(),
        builder: "/bin/sh".into(),
        args: Vec::from([
            "-c".into(),
            outputs
                .iter()
                .map(|(name, drv_path)| {
                    let dest =
                        Placeholder::standard_output(&OutputName::from_str(name).unwrap()).render();
                    let src = Placeholder::ca_output(drv_path, &OutputName::default()).render();

                    let mut buf = bytes::BytesMut::new();
                    buf.extend(ln.real_path.clone_bytes());
                    buf.extend_from_slice(" -s ".as_bytes());
                    buf.extend(src.into_bytes());
                    buf.extend(" ".as_bytes());
                    buf.extend(dest.into_bytes());
                    buf.extend(";".as_bytes());

                    buf.into()
                })
                .fold(bytes::BytesMut::new(), |mut acc, rhs: bytes::Bytes| {
                    acc.extend(rhs);
                    acc
                })
                .into(),
        ]),
        env: BTreeMap::new(),
        structured_attrs: None,
    };

    let wrapper_drv_path =
        add_drv_to_store(store, store_dir, wrapper_drv, "cargo-dyndrv-build").await?;

    store
        .submit_output(
            &SingleDerivedPath::Opaque(wrapper_drv_path),
            &OutputName::default(),
        )
        .await?;

    Ok(())
}
