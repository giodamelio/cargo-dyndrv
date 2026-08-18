use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    str::FromStr,
    sync::{Arc, OnceLock},
};

use color_eyre::eyre;
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

pub async fn submit_wrapper<T: DaemonStore>(
    store: &mut T,
    store_dir: &StoreDir,
    ln: &Tool,
    output_name: &str,
    orig_drv: &StorePath,
) -> eyre::Result<()> {
    let name = format!("cargo-dyndrv-build-{}", output_name);
    // We're only allowed to submit if the name is correct.
    // The easiest way to do this without breaking the inner loop is a symlink
    let wrapper_drv = Derivation {
        name: StorePathName::from_str(&name)?,
        outputs: BTreeMap::from([(
            OutputName::default(),
            DerivationOutput::CAFloating(ContentAddressMethodAlgorithm::NixArchive(SHA256)),
        )]),
        inputs: BTreeSet::from([
            SingleDerivedPath::Opaque(ln.store_path.clone()),
            SingleDerivedPath::Built {
                drv_path: Arc::new(SingleDerivedPath::Opaque(orig_drv.clone())),
                output: OutputName::default(),
            },
        ]),
        platform: "x86_64-linux".into(),
        builder: ln.real_path.clone_bytes(),
        args: vec![
            "-s".into(),
            // We can't reference environment variables, so use placeholders
            Placeholder::standard_output(&OutputName::default())
                .render()
                .into_bytes(),
            Placeholder::ca_output(orig_drv, &OutputName::default())
                .render()
                .into_bytes(),
        ],
        env: BTreeMap::new(),
        structured_attrs: None,
    };

    let wrapper_drv_path = add_drv_to_store(store, store_dir, wrapper_drv, &name).await?;

    store
        .submit_output(
            &SingleDerivedPath::Opaque(wrapper_drv_path),
            &OutputName::from_str(&name)?,
        )
        .await?;

    Ok(())
}
