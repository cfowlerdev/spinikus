//! Reaching a destination's bucket from the control plane, which tests destinations and
//! deletes backups; agents upload them with the same settings ([`proto`]).

use std::sync::Arc;

use object_store::{ObjectStore, aws::AmazonS3Builder};
use spinikus_core::{backup::StorageDestinationFields, secret::Secret};
use spinikus_proto::agent::v1 as pb;

/// Where the control plane reaches destinations: their buckets, or one store for every
/// destination in tests.
#[derive(Clone, Default)]
pub(crate) enum BackupStores {
    #[default]
    Destination,
    Fixed(Arc<dyn ObjectStore>),
}

impl BackupStores {
    pub(crate) fn store(
        &self,
        fields: &StorageDestinationFields,
        secret: &Secret,
    ) -> Result<Arc<dyn ObjectStore>, object_store::Error> {
        match self {
            Self::Destination => s3(fields, secret),
            Self::Fixed(store) => Ok(store.clone()),
        }
    }
}

fn s3(fields: &StorageDestinationFields, secret: &Secret) -> Result<Arc<dyn ObjectStore>, object_store::Error> {
    // object_store uses rustls without choosing a provider; this is the one the control plane uses.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut builder = AmazonS3Builder::new()
        .with_bucket_name(&fields.bucket)
        .with_region(&fields.region)
        .with_access_key_id(&fields.access_key_id)
        .with_secret_access_key(secret.expose())
        .with_virtual_hosted_style_request(!fields.path_style);
    if let Some(endpoint) = &fields.endpoint {
        builder = builder.with_endpoint(endpoint).with_allow_http(endpoint.starts_with("http://"));
    }
    Ok(Arc::new(builder.build()?))
}

/// The destination as the agent needs it, credentials included.
pub(crate) fn proto(fields: &StorageDestinationFields, secret: &Secret) -> pb::StorageDestination {
    pb::StorageDestination {
        endpoint: fields.endpoint.clone().unwrap_or_default(),
        region: fields.region.clone(),
        bucket: fields.bucket.clone(),
        path_style: fields.path_style,
        access_key_id: fields.access_key_id.clone(),
        secret_access_key: secret.expose().to_string(),
    }
}
