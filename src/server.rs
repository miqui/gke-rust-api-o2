//! Server assembly: gRPC (MessageService, AuthorService, grpc.health.v1, reflection).

use crate::{
    cache::MessageCache,
    grpc::{AUTHOR_SERVICE, AuthorGrpc, MESSAGE_SERVICE, MessageGrpc},
    metrics::Metrics,
    pb::{
        self, author_service_server::AuthorServiceServer,
        message_service_server::MessageServiceServer,
    },
    service::{Authors, Messages},
};
use sqlx::PgPool;
use std::{future::Future, net::SocketAddr};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;

/// Bounds the cost of one request: the largest valid request is a few KB (Java: 16KB).
pub const MAX_INBOUND_MESSAGE: usize = 16 * 1024;

pub struct GrpcDeps {
    pub pool: PgPool,
    pub cache: MessageCache,
    pub metrics: Metrics,
}

/// Serves gRPC on `listener` until `shutdown` resolves, then drains in-flight calls (the caller
/// bounds the drain time). `health_ready` lets the caller flip grpc.health.v1 to NOT_SERVING
/// before the drain starts.
pub async fn serve_grpc(
    listener: TcpListener,
    deps: GrpcDeps,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), tonic::transport::Error> {
    let (reporter, health_service) = tonic_health::server::health_reporter();
    for name in ["", MESSAGE_SERVICE, AUTHOR_SERVICE] {
        reporter
            .set_service_status(name, tonic_health::ServingStatus::Serving)
            .await;
    }

    let reflection_v1 = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(pb::FILE_DESCRIPTOR_SET)
        .register_encoded_file_descriptor_set(tonic_health::pb::FILE_DESCRIPTOR_SET)
        .build_v1()
        .expect("valid descriptor set");
    let reflection_v1alpha = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(pb::FILE_DESCRIPTOR_SET)
        .register_encoded_file_descriptor_set(tonic_health::pb::FILE_DESCRIPTOR_SET)
        .build_v1alpha()
        .expect("valid descriptor set");

    let messages = MessageServiceServer::new(MessageGrpc {
        messages: Messages::new(deps.pool.clone(), deps.cache),
        metrics: deps.metrics.clone(),
    })
    .max_decoding_message_size(MAX_INBOUND_MESSAGE);
    let authors = AuthorServiceServer::new(AuthorGrpc {
        authors: Authors::new(deps.pool),
        metrics: deps.metrics,
    })
    .max_decoding_message_size(MAX_INBOUND_MESSAGE);

    let shutdown_reporter = reporter.clone();
    tonic::transport::Server::builder()
        .add_service(health_service)
        .add_service(reflection_v1)
        .add_service(reflection_v1alpha)
        .add_service(messages)
        .add_service(authors)
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
            shutdown.await;
            for name in ["", MESSAGE_SERVICE, AUTHOR_SERVICE] {
                shutdown_reporter
                    .set_service_status(name, tonic_health::ServingStatus::NotServing)
                    .await;
            }
        })
        .await
}

pub async fn bind(port: u16) -> std::io::Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    let addr = listener.local_addr()?;
    Ok((listener, addr))
}
