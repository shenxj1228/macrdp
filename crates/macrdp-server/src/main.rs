use anyhow::{Context, Result};
use clap::Parser;
use macrdp_core::{ServerConfig, ServerEventHandler, ServerStatus};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

#[derive(Parser, Debug)]
#[command(name = "macrdp", about = "macOS RDP Server")]
struct Cli {
    /// TCP port to listen on
    #[arg(short, long, default_value_t = 3389)]
    port: u16,

    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Target frame rate (30, 60, or 120)
    #[arg(long)]
    frame_rate: Option<u32>,

    /// Log level: trace, debug, info, warn, error
    #[arg(long)]
    log_level: Option<String>,

    /// Enable performance statistics collection and print summary on exit
    #[arg(long, default_value_t = false)]
    perf: bool,
}

impl Cli {
    /// Load config from file, then apply CLI overrides
    fn load_config(&self) -> Result<ServerConfig> {
        let mut config = ServerConfig::load_from_file(self.config.as_deref())?;
        if self.port != 3389 {
            config.port = self.port;
        }
        if let Some(fps) = self.frame_rate {
            config.frame_rate = fps;
        }
        if let Some(level) = &self.log_level {
            config.log_level = Some(level.clone());
        }
        Ok(config)
    }
}

/// CLI event handler: wakes `main` when the server thread exits with an error
/// (e.g. TLS or bind failure), so the process terminates instead of waiting
/// for Ctrl-C forever.
#[derive(Clone)]
struct CliHandler {
    exited: Arc<Notify>,
    error: Arc<Mutex<Option<String>>>,
}

impl ServerEventHandler for CliHandler {
    fn on_status_change(&self, status: ServerStatus) {
        if !status.running {
            *self.error.lock().unwrap() = Some(status.state);
            self.exited.notify_one();
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = cli.load_config()?;

    // Initialize logging — write to both stderr and file
    let log_level = config.log_level.as_deref().unwrap_or("info");
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level));

    let log_path = std::env::current_dir().unwrap_or_default().join("macrdp.log");
    let log_file = std::fs::File::create(&log_path)?;

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(Mutex::new(log_file)),
        )
        .init();

    eprintln!("Log file: {}", log_path.display());

    tracing::info!(?config, "macrdp server starting");

    // Create shared performance stats (enabled via --perf flag)
    let perf_stats = if cli.perf {
        tracing::info!("Performance statistics collection enabled (--perf)");
        Some(macrdp_core::new_shared_perf_stats(true))
    } else {
        None
    };

    let handler = CliHandler {
        exited: Arc::new(Notify::new()),
        error: Arc::new(Mutex::new(None)),
    };

    let handle = macrdp_core::start_server_with_perf(config, handler.clone(), perf_stats.clone())
        .await
        .context("Failed to start RDP server")?;

    tracing::info!(port = handle.port(), "RDP server listening");
    tracing::info!("Connect using an RDP client (e.g., Windows mstsc or Microsoft Remote Desktop)");

    // Run until Ctrl-C or server-thread exit; print perf summary if enabled
    let result = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received Ctrl-C — shutting down");
            Ok(())
        }
        _ = handler.exited.notified() => {
            let msg = handler.error.lock().unwrap().take().unwrap_or_default();
            Err(anyhow::anyhow!("RDP server error: {msg}"))
        }
    };

    handle.stop().await?;

    if let Some(ps) = &perf_stats {
        ps.lock().unwrap().print_summary();
    }

    result
}
