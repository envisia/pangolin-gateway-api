//! Server-Side Apply + metadata stamping shared by every kind.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Resource;
use kube::api::{Api, Patch, PatchParams};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{debug, info};

use crate::config::Config;

/// Labels applied to every K8s object the controller writes.
pub fn owner_labels(cfg: &Config, _name: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(
        cfg.managed_label_key.clone(),
        cfg.managed_label_value.clone(),
    );
    labels.insert(
        cfg.instance_label_key.clone(),
        cfg.instance_label_value.clone(),
    );
    labels
}

/// `ObjectMeta` populated with the controller's labels and the managed annotation,
/// plus any caller-supplied extras. User-supplied entries win over the managed one
/// only if they target the same key — this lets operators relabel things if needed.
pub fn managed_metadata_with(
    cfg: &Config,
    name: &str,
    labels: BTreeMap<String, String>,
    extra_annotations: &BTreeMap<String, String>,
) -> ObjectMeta {
    let mut annotations = BTreeMap::new();
    annotations.insert(
        cfg.managed_annotation_key.clone(),
        cfg.managed_annotation_value.clone(),
    );
    for (k, v) in extra_annotations {
        annotations.insert(k.clone(), v.clone());
    }
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: Some(cfg.namespace.clone()),
        labels: Some(labels),
        annotations: Some(annotations),
        ..Default::default()
    }
}

/// `ObjectMeta` populated with the controller's labels and only the managed annotation.
pub fn managed_metadata(
    cfg: &Config,
    name: &str,
    labels: BTreeMap<String, String>,
) -> ObjectMeta {
    managed_metadata_with(cfg, name, labels, &BTreeMap::new())
}

/// Apply a single object via Server-Side Apply. `T` must be a namespaced Kubernetes
/// resource whose `DynamicType` is `()` — true for built-ins and for kube_derive-generated
/// CustomResources.
pub async fn ssa_apply<T>(
    api: &Api<T>,
    cfg: &Config,
    obj: &T,
) -> Result<()>
where
    T: Resource<DynamicType = ()> + Clone + Serialize + DeserializeOwned + std::fmt::Debug,
{
    let name = obj
        .meta()
        .name
        .as_deref()
        .context("object has no metadata.name")?;

    if cfg.read_only {
        info!(kind = %T::kind(&()), name = %name, "read-only mode: skipping apply");
        return Ok(());
    }

    let params = PatchParams::apply(&cfg.field_manager).force();
    let res = api.patch(name, &params, &Patch::Apply(obj)).await;
    match res {
        Ok(_) => {
            debug!(kind = %T::kind(&()), name = %name, "applied");
            Ok(())
        }
        Err(e) => Err(e).with_context(|| {
            format!(
                "applying {kind}/{name}",
                kind = T::kind(&()),
                name = name
            )
        }),
    }
}
