//! Iron Proxy adapters: the external gRPC transform service, bounded request
//! correlation state, and the response-retry HTTP callbacks.

mod attempts;
pub(super) mod retry;
mod tls;
mod transform;

pub(super) use attempts::AttemptStore;
pub(crate) use tls::{IronTlsPaths, ensure_iron_tls, purge_iron_tls};
pub(super) use transform::IronTransform;

pub(super) mod proto {
    tonic::include_proto!("transform.v1");
}
