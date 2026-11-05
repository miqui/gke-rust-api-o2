//! Prometheus metrics (scraped from the management port at `/metrics`).
//!
//! * `grpc_server_seconds{rpc_service,rpc_method,grpc_status_code}`: latency histogram of every
//!   application RPC (same labels/buckets as the Micrometer `grpc.server` timer; the `_count`
//!   series is the throughput). Health and reflection calls are not recorded.
//! * `grpc_errors_total{rpc_service,rpc_method,grpc_status_code,error_code}`: non-OK calls;
//!   `error_code` is the `ErrorInfo` reason (BAD_USER_INPUT, NOT_FOUND, CONFLICT,
//!   INTERNAL_SERVER_ERROR).
//! * `message_cache_seconds{operation,outcome}` / `message_cache_errors_total{operation}`: Redis
//!   round trips (successor of `hazelcast_cache`); `outcome` is hit|miss|ok|error.

use prometheus::{CounterVec, Encoder, HistogramOpts, HistogramVec, Opts, Registry, TextEncoder};

const GRPC_BUCKETS: &[f64] = &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0];
const CACHE_BUCKETS: &[f64] = &[0.001, 0.002, 0.005, 0.01, 0.025, 0.05, 0.1];

#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    grpc_seconds: HistogramVec,
    grpc_errors: CounterVec,
    cache_seconds: HistogramVec,
    cache_errors: CounterVec,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let grpc_seconds = HistogramVec::new(
            HistogramOpts::new("grpc_server_seconds", "Latency of application gRPC calls")
                .buckets(GRPC_BUCKETS.to_vec()),
            &["rpc_service", "rpc_method", "grpc_status_code"],
        )
        .expect("valid metric");
        let grpc_errors = CounterVec::new(
            Opts::new(
                "grpc_errors_total",
                "gRPC calls that ended with a non-OK status, by method and error code",
            ),
            &[
                "rpc_service",
                "rpc_method",
                "grpc_status_code",
                "error_code",
            ],
        )
        .expect("valid metric");
        let cache_seconds = HistogramVec::new(
            HistogramOpts::new(
                "message_cache_seconds",
                "Latency of message cache operations",
            )
            .buckets(CACHE_BUCKETS.to_vec()),
            &["operation", "outcome"],
        )
        .expect("valid metric");
        let cache_errors = CounterVec::new(
            Opts::new(
                "message_cache_errors_total",
                "Cache operations that failed and were skipped",
            ),
            &["operation"],
        )
        .expect("valid metric");
        for c in [
            Box::new(grpc_seconds.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(grpc_errors.clone()),
            Box::new(cache_seconds.clone()),
            Box::new(cache_errors.clone()),
        ] {
            registry.register(c).expect("unique metric");
        }
        Metrics {
            registry,
            grpc_seconds,
            grpc_errors,
            cache_seconds,
            cache_errors,
        }
    }

    pub fn record_rpc(
        &self,
        service: &str,
        method: &str,
        code: &str,
        error_code: Option<&str>,
        seconds: f64,
    ) {
        self.grpc_seconds
            .with_label_values(&[service, method, code])
            .observe(seconds);
        if let Some(error_code) = error_code {
            self.grpc_errors
                .with_label_values(&[service, method, code, error_code])
                .inc();
        }
    }

    pub fn record_cache(&self, operation: &str, outcome: &str, seconds: f64) {
        self.cache_seconds
            .with_label_values(&[operation, outcome])
            .observe(seconds);
    }

    pub fn record_cache_error(&self, operation: &str) {
        self.cache_errors.with_label_values(&[operation]).inc();
    }

    /// Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&self.registry.gather(), &mut buf)
            .expect("encode metrics");
        String::from_utf8(buf).expect("utf8 metrics")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_recorded_series() {
        let m = Metrics::new();
        m.record_rpc(
            "message.v1.MessageService",
            "GetMessage",
            "NOT_FOUND",
            Some("NOT_FOUND"),
            0.02,
        );
        m.record_rpc("message.v1.MessageService", "GetMessage", "OK", None, 0.001);
        let out = m.render();
        assert!(out.contains("grpc_errors_total{error_code=\"NOT_FOUND\",grpc_status_code=\"NOT_FOUND\",rpc_method=\"GetMessage\",rpc_service=\"message.v1.MessageService\"} 1"), "{out}");
        assert!(
            out.contains("grpc_server_seconds_count{grpc_status_code=\"OK\""),
            "{out}"
        );
        assert!(out.contains("le=\"0.005\""));
    }
}
