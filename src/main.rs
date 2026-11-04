use anyhow::Context;
use message_service::{
    cache::MessageCache,
    config::Config,
    management::{self, MgmtState},
    metrics::Metrics,
    migrate, seed,
    server::{self, GrpcDeps},
    telemetry,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use std::{
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    let _telemetry = telemetry::init(&cfg)?;
    tracing::info!(
        grpc_port = cfg.grpc_port,
        management_port = cfg.management_port,
        "starting message-service"
    );

    let options = PgConnectOptions::new()
        .host(&cfg.db.host)
        .port(cfg.db.port)
        .database(&cfg.db.name)
        .username(&cfg.db.user)
        .password(&cfg.db.password)
        .ssl_mode(
            PgSslMode::from_str(&cfg.db.sslmode)
                .map_err(|e| anyhow::anyhow!("DB_SSLMODE={:?}: {e}", cfg.db.sslmode))?,
        );
    let pool = PgPoolOptions::new()
        .max_connections(cfg.db.pool_max)
        .min_connections(2.min(cfg.db.pool_max))
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await
        .context("connecting to PostgreSQL")?;

    let applied = migrate::run(&pool).await.context("running migrations")?;
    tracing::info!(?applied, "migrations up to date");
    if cfg.seed_enabled && seed::seed(&pool).await.context("seeding welcome message")? {
        tracing::info!("welcome message created");
    }

    let metrics = Metrics::new();
    let cache = match &cfg.redis {
        Some(r) => MessageCache::new(r, metrics.clone())?,
        None => {
            tracing::warn!("REDIS_HOST not set: running without a cache");
            MessageCache::disabled()
        }
    };

    let shutting_down = Arc::new(AtomicBool::new(false));
    let (grpc_listener, grpc_addr) = server::bind(cfg.grpc_port).await?;
    let mgmt_listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.management_port)).await?;
    tracing::info!(%grpc_addr, mgmt = %mgmt_listener.local_addr()?, "listening");

    let (stop_tx, stop_rx) = watch::channel(false);
    let mgmt_state = MgmtState {
        pool: pool.clone(),
        cache: cache.clone(),
        metrics: metrics.clone(),
        shutting_down: shutting_down.clone(),
    };
    let mut mgmt_stop = stop_rx.clone();
    let mgmt = tokio::spawn(async move {
        axum::serve(mgmt_listener, management::router(mgmt_state))
            .with_graceful_shutdown(async move {
                let _ = mgmt_stop.wait_for(|s| *s).await;
            })
            .await
    });

    // SIGTERM (k8s) or Ctrl-C: stop being ready, then drain gRPC for at most the grace period.
    let grace = cfg.shutdown_grace;
    let flag = shutting_down.clone();
    let (term_tx, term_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        wait_for_signal().await;
        flag.store(true, Ordering::Relaxed);
        tracing::info!(
            grace_seconds = grace.as_secs(),
            "shutdown signal received; draining"
        );
        let _ = term_tx.send(());
    });

    let grpc = tokio::spawn(server::serve_grpc(
        grpc_listener,
        GrpcDeps {
            pool: pool.clone(),
            cache,
            metrics,
        },
        async move {
            let _ = term_rx.await;
        },
    ));
    tokio::pin!(grpc);
    // Wait for the signal, then bound the drain.
    let result = tokio::select! {
        r = &mut grpc => r,
        () = async {
            while !shutting_down.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            tokio::time::sleep(grace).await;
        } => {
            tracing::warn!("grace period elapsed; forcing shutdown");
            Ok(Ok(()))
        }
    };
    let _ = stop_tx.send(true);
    let _ = mgmt.await;
    pool.close().await;
    tracing::info!("stopped");
    result??;
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
