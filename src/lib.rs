//! Library API for `pangolin-gateway-controller`.
//!
//! Every module the binary uses is `pub` here so integration tests in `tests/`
//! and other downstream consumers (an in-cluster operator-runner crate, future
//! CRD-only smoke tests, etc.) can drive the transform pipeline without going
//! through the binary entrypoint.

pub mod apply;
pub mod config;
pub mod envoy_gateway;
pub mod gc;
pub mod pangolin;
pub mod reconcile;
pub mod transform;
