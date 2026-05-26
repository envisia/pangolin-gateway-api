//! Mark-and-sweep garbage collection: anything carrying our managed labels that's
//! NOT in the desired set gets deleted.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use kube::Resource;
use kube::api::{Api, DeleteParams, ListParams};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{info, warn};

use crate::config::Config;

pub async fn sweep<T>(api: &Api<T>, cfg: &Config, desired_names: &BTreeSet<String>) -> Result<()>
where
    T: Resource<DynamicType = ()> + Clone + Serialize + DeserializeOwned + std::fmt::Debug,
{
    let lp = ListParams::default().labels(&cfg.managed_selector());
    let existing = api
        .list(&lp)
        .await
        .with_context(|| format!("listing existing {} for GC", T::kind(&())))?;

    for item in existing.items {
        let Some(name) = item.meta().name.clone() else {
            continue;
        };
        if desired_names.contains(&name) {
            continue;
        }
        if cfg.read_only {
            info!(kind = %T::kind(&()), name = %name, "read-only mode: would delete orphan");
            continue;
        }
        info!(kind = %T::kind(&()), name = %name, "deleting orphan");
        if let Err(e) = api.delete(&name, &DeleteParams::default()).await
            && !is_not_found(&e)
        {
            warn!(kind = %T::kind(&()), name = %name, error = ?e, "failed to delete orphan");
        }
    }
    Ok(())
}

fn is_not_found(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(s) if s.code == 404)
}
