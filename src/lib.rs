//! Message/Author gRPC service: a Rust (tonic) port of the Spring Boot `message-service`.
//!
//! Layering mirrors the Java original: `grpc` (validation + proto plumbing) -> `service`
//! (use cases, explicit transactions, cache orchestration) -> `repo` (SQL) / `cache` (Redis).

pub mod cache;
pub mod config;
pub mod error;
pub mod grpc;
pub mod management;
pub mod metrics;
pub mod migrate;
pub mod repo;
pub mod seed;
pub mod server;
pub mod service;
pub mod telemetry;
pub mod validation;

/// Generated protobuf/gRPC code for package `message.v1` (contract: proto/message/v1).
pub mod pb {
    #![allow(clippy::all, clippy::pedantic, missing_docs)]
    tonic::include_proto!("message.v1");

    /// Encoded `FileDescriptorSet` (with imports) served by server reflection.
    pub const FILE_DESCRIPTOR_SET: &[u8] =
        include_bytes!(concat!(env!("OUT_DIR"), "/message_service_descriptor.bin"));
}
