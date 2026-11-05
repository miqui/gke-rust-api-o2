//! gRPC adapters: proto plumbing + hand validation only (the Java `*GrpcService` classes), plus
//! the per-RPC instrumentation of `RpcMetricsInterceptor` (metrics, one structured log line per
//! RPC, trace span continuing the caller's W3C trace context).

use crate::{
    error::{ApiError, error_code_of},
    metrics::Metrics,
    pb::{
        Author, CreateAuthorRequest, CreateMessageRequest, DeleteAuthorRequest,
        DeleteMessageRequest, GetAuthorRequest, GetAuthorResponse, GetMessageRequest,
        ListAuthorsRequest, ListAuthorsResponse, ListMessagesRequest, ListMessagesResponse,
        Message, UpdateAuthorRequest, UpdateMessageRequest, author_service_server::AuthorService,
        message_service_server::MessageService,
    },
    service::{Authors, Messages},
    validation::{CONTENT_MAX, NAME_MAX, TITLE_MAX, Violations},
};
use opentelemetry::{
    propagation::{Extractor, TextMapPropagator},
    trace::TraceContextExt,
};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use std::{future::Future, time::Instant};
use tonic::{Request, Response, Status, metadata::MetadataMap};
use tracing::{Instrument, field::Empty};
use tracing_opentelemetry::OpenTelemetrySpanExt;

pub const MESSAGE_SERVICE: &str = "message.v1.MessageService";
pub const AUTHOR_SERVICE: &str = "message.v1.AuthorService";

struct MetadataExtractor<'a>(&'a MetadataMap);

impl Extractor for MetadataExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0
            .keys()
            .filter_map(|k| match k {
                tonic::metadata::KeyRef::Ascii(k) => Some(k.as_str()),
                tonic::metadata::KeyRef::Binary(_) => None,
            })
            .collect()
    }
}

/// Runs one unary RPC body: span (child of the caller's trace), error mapping to
/// `google.rpc.Status`, `grpc_server_seconds` / `grpc_errors_total`, one access-log line.
/// Errors are answered from inside the call so the real status is what every layer records.
async fn instrument<T, F>(
    metrics: &Metrics,
    service: &'static str,
    method: &'static str,
    headers: &MetadataMap,
    body: F,
) -> Result<Response<T>, Status>
where
    F: Future<Output = Result<T, ApiError>>,
{
    let span = tracing::info_span!(
        "rpc",
        otel.name = %format!("{service}/{method}"),
        rpc.system = "grpc",
        rpc.service = service,
        rpc.method = method,
        trace_id = Empty,
    );
    let parent = TraceContextPropagator::new().extract(&MetadataExtractor(headers));
    let _ = span.set_parent(parent);
    {
        let ctx = span.context();
        let sc = ctx.span().span_context().clone();
        if sc.is_valid() {
            span.record("trace_id", tracing::field::display(sc.trace_id()));
        }
    }

    let start = Instant::now();
    let outcome = body.instrument(span.clone()).await;
    let elapsed = start.elapsed();
    let _guard = span.enter();

    let result = match outcome {
        Ok(v) => Ok(Response::new(v)),
        Err(e) => {
            if let ApiError::Internal(detail) = &e {
                tracing::error!(error = %detail, "unhandled exception");
            }
            Err(Status::from(e))
        }
    };
    let (code, error_code) = match &result {
        Ok(_) => ("OK".to_owned(), None),
        Err(s) => (code_name(s.code()), Some(error_code_of(s))),
    };
    metrics.record_rpc(
        service,
        method,
        &code,
        error_code.as_deref(),
        elapsed.as_secs_f64(),
    );
    tracing::info!(
        rpc_method = %format!("{service}/{method}"),
        grpc_status = %code,
        duration_ms = (elapsed.as_secs_f64() * 100_000.0).round() / 100.0,
        "rpc"
    );
    result
}

/// gRPC canonical upper-snake status names (what Micrometer/grpc-java label with).
pub fn code_name(code: tonic::Code) -> String {
    use tonic::Code::*;
    match code {
        Ok => "OK",
        Cancelled => "CANCELLED",
        Unknown => "UNKNOWN",
        InvalidArgument => "INVALID_ARGUMENT",
        DeadlineExceeded => "DEADLINE_EXCEEDED",
        NotFound => "NOT_FOUND",
        AlreadyExists => "ALREADY_EXISTS",
        PermissionDenied => "PERMISSION_DENIED",
        ResourceExhausted => "RESOURCE_EXHAUSTED",
        FailedPrecondition => "FAILED_PRECONDITION",
        Aborted => "ABORTED",
        OutOfRange => "OUT_OF_RANGE",
        Unimplemented => "UNIMPLEMENTED",
        Internal => "INTERNAL",
        Unavailable => "UNAVAILABLE",
        DataLoss => "DATA_LOSS",
        Unauthenticated => "UNAUTHENTICATED",
    }
    .to_owned()
}

#[derive(Clone)]
pub struct MessageGrpc {
    pub messages: Messages,
    pub metrics: Metrics,
}

#[derive(Clone)]
pub struct AuthorGrpc {
    pub authors: Authors,
    pub metrics: Metrics,
}

#[tonic::async_trait]
impl MessageService for MessageGrpc {
    async fn list_messages(
        &self,
        request: Request<ListMessagesRequest>,
    ) -> Result<Response<ListMessagesResponse>, Status> {
        let (md, _, req) = request.into_parts();
        instrument(&self.metrics, MESSAGE_SERVICE, "ListMessages", &md, async {
            let mut v = Violations::new();
            let limit = v.limit("limit", req.limit);
            let offset = v.offset("offset", req.offset.map(i64::from));
            v.finish()?;
            self.messages.list(limit, offset).await
        })
        .await
    }

    async fn get_message(
        &self,
        request: Request<GetMessageRequest>,
    ) -> Result<Response<Message>, Status> {
        let (md, _, req) = request.into_parts();
        instrument(&self.metrics, MESSAGE_SERVICE, "GetMessage", &md, async {
            let mut v = Violations::new();
            let id = v.uuid("id", &req.id);
            v.finish()?;
            self.messages.get(id.expect("validated")).await
        })
        .await
    }

    async fn create_message(
        &self,
        request: Request<CreateMessageRequest>,
    ) -> Result<Response<Message>, Status> {
        let (md, _, req) = request.into_parts();
        instrument(
            &self.metrics,
            MESSAGE_SERVICE,
            "CreateMessage",
            &md,
            async {
                let mut v = Violations::new();
                let title = v.text("title", &req.title, TITLE_MAX);
                let content = v.text("content", &req.content, CONTENT_MAX);
                let author_id = v.uuid("author_id", &req.author_id);
                v.finish()?;
                self.messages
                    .create(&title, &content, author_id.expect("validated"))
                    .await
            },
        )
        .await
    }

    async fn update_message(
        &self,
        request: Request<UpdateMessageRequest>,
    ) -> Result<Response<Message>, Status> {
        let (md, _, req) = request.into_parts();
        instrument(
            &self.metrics,
            MESSAGE_SERVICE,
            "UpdateMessage",
            &md,
            async {
                let mut v = Violations::new();
                let id = v.uuid("id", &req.id);
                let title = req.title.as_deref().map(|t| v.text("title", t, TITLE_MAX));
                let content = v.text("content", &req.content, CONTENT_MAX);
                match req.version {
                    None => v.add("version", "version is required"),
                    Some(n) if n < 0 => v.add("version", "version must be 0 or greater"),
                    Some(_) => {}
                }
                v.finish()?;
                self.messages
                    .update(
                        id.expect("validated"),
                        title.as_deref(),
                        &content,
                        req.version.expect("validated"),
                    )
                    .await
            },
        )
        .await
    }

    async fn delete_message(
        &self,
        request: Request<DeleteMessageRequest>,
    ) -> Result<Response<()>, Status> {
        let (md, _, req) = request.into_parts();
        instrument(
            &self.metrics,
            MESSAGE_SERVICE,
            "DeleteMessage",
            &md,
            async {
                let mut v = Violations::new();
                let id = v.uuid("id", &req.id);
                v.finish()?;
                self.messages.delete(id.expect("validated")).await
            },
        )
        .await
    }
}

#[tonic::async_trait]
impl AuthorService for AuthorGrpc {
    async fn list_authors(
        &self,
        request: Request<ListAuthorsRequest>,
    ) -> Result<Response<ListAuthorsResponse>, Status> {
        let (md, _, req) = request.into_parts();
        instrument(&self.metrics, AUTHOR_SERVICE, "ListAuthors", &md, async {
            let mut v = Violations::new();
            let limit = v.limit("limit", req.limit);
            let offset = v.offset("offset", req.offset.map(i64::from));
            v.finish()?;
            self.authors.list(limit, offset).await
        })
        .await
    }

    async fn get_author(
        &self,
        request: Request<GetAuthorRequest>,
    ) -> Result<Response<GetAuthorResponse>, Status> {
        let (md, _, req) = request.into_parts();
        instrument(&self.metrics, AUTHOR_SERVICE, "GetAuthor", &md, async {
            let mut v = Violations::new();
            let id = v.uuid("id", &req.id);
            let limit = v.limit("messages_limit", req.messages_limit);
            let offset = v.offset("messages_offset", req.messages_offset);
            v.finish()?;
            self.authors
                .get(id.expect("validated"), req.include_messages, limit, offset)
                .await
        })
        .await
    }

    async fn create_author(
        &self,
        request: Request<CreateAuthorRequest>,
    ) -> Result<Response<Author>, Status> {
        let (md, _, req) = request.into_parts();
        instrument(&self.metrics, AUTHOR_SERVICE, "CreateAuthor", &md, async {
            let mut v = Violations::new();
            let name = v.text("name", &req.name, NAME_MAX);
            let email = v.email("email", &req.email);
            v.finish()?;
            self.authors.create(&name, &email).await
        })
        .await
    }

    async fn update_author(
        &self,
        request: Request<UpdateAuthorRequest>,
    ) -> Result<Response<Author>, Status> {
        let (md, _, req) = request.into_parts();
        instrument(&self.metrics, AUTHOR_SERVICE, "UpdateAuthor", &md, async {
            let mut v = Violations::new();
            let id = v.uuid("id", &req.id);
            let name = req.name.as_deref().map(|n| v.text("name", n, NAME_MAX));
            let email = req.email.as_deref().map(|e| v.email("email", e));
            v.finish()?;
            self.authors
                .update(id.expect("validated"), name.as_deref(), email.as_deref())
                .await
        })
        .await
    }

    async fn delete_author(
        &self,
        request: Request<DeleteAuthorRequest>,
    ) -> Result<Response<()>, Status> {
        let (md, _, req) = request.into_parts();
        instrument(&self.metrics, AUTHOR_SERVICE, "DeleteAuthor", &md, async {
            let mut v = Violations::new();
            let id = v.uuid("id", &req.id);
            v.finish()?;
            self.authors.delete(id.expect("validated")).await
        })
        .await
    }
}
