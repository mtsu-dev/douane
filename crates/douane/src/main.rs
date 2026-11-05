use anyhow::{Context, Result};
use clap::Parser;
use douane::{settings::Config, state::AppState};
use std::{path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(
    version,
    about = "Subscription proxy with OAuth accounts, quota routing, and an embedded dashboard"
)]
struct Cli {
    #[arg(long, default_value = "douane.toml")]
    config: PathBuf,
    #[arg(long)]
    listen: Option<String>,
    #[arg(long)]
    auth_dir: Option<PathBuf>,
    #[arg(long)]
    check: bool,
}
#[tokio::main(worker_threads = 4)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "douane=info,tower_http=warn".into()),
        )
        .init();
    let cli = Cli::parse();
    let path = std::path::absolute(&cli.config)?;
    let mut config = if path.exists() {
        Config::load(&path)?
    } else {
        Config::default()
    };
    if let Some(listen) = cli.listen {
        config.server.listen = listen;
    }
    if let Some(auth_dir) = cli.auth_dir {
        config.server.auth_dir = auth_dir;
    }
    config.settings.validate()?;
    let listen = config.server.listen.clone();
    if cli.check {
        if let Some(name) = &config.server.management_token_env {
            std::env::var(name).with_context(|| {
                format!("management token environment variable {name} is unavailable")
            })?;
        }
        println!("configuration valid");
        return Ok(());
    }
    let state = AppState::new(config, Some(path))?;
    state.refresh_models().await;
    let poller = state.clone();
    let task = tokio::spawn(async move {
        // Each account keeps its own schedule: the refresh interval when its
        // reads succeed, a widening retry delay when they fail. Snapshots are
        // saved every 5 minutes and at shutdown.
        let mut pass = 0u64;
        loop {
            pass += 1;
            poller.refresh_due_quotas().await;
            poller.sample_quotas().await;
            if pass.is_multiple_of(20) {
                poller.quotas.save().await;
            }
            // Every catalog is read again each hour, so a model a provider
            // adds or retires shows without a restart; a live model listing
            // that failed is read again every minute.
            if pass.is_multiple_of(240) {
                let retention = douane::quota_history::RETENTION_DAYS * 86400;
                poller
                    .ledger
                    .prune_samples(time::OffsetDateTime::now_utc().unix_timestamp() - retention);
                poller.refresh_models().await;
            } else if pass.is_multiple_of(4) {
                for pool in poller.stale_pools() {
                    poller.refresh_models_in(Some(&pool)).await;
                }
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let clients = state.clients.list().len();
    tracing::info!(listen=%listen,accounts=state.accounts.list().await.len(),clients,"subscription proxy ready");
    if clients == 0 {
        tracing::warn!(
            "no client yet: every API call is refused until one is created on the Clients page or with `douanectl client add`"
        );
    }
    axum::serve(listener, douane::router(state.clone()))
        .with_graceful_shutdown(shutdown())
        .await?;
    task.abort();
    state.quotas.save().await;
    state.ledger.flush().await;
    Ok(())
}
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
