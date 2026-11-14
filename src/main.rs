use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use qr_live::languages;
use qr_live::relay::{self, Mode, Relay, RunConfig};
use qr_live::server::{self, Assets};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ModeArg {
    /// Animate the reference programs without executing anything.
    Replay,
    /// Execute every step for real.
    Exec,
}

/// Serves a live visualisation of the quine relay.
#[derive(Parser, Debug)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen: String,
    #[arg(long, value_enum, default_value = "replay")]
    mode: ModeArg,
    /// Directory holding the reference programs.
    #[arg(long, default_value = "./reference")]
    reference_dir: PathBuf,
    /// Time per step in replay mode, in milliseconds.
    #[arg(long, default_value_t = 1500)]
    step_ms: u64,
    /// Minimum time each step is shown in exec mode, in milliseconds.
    #[arg(long, default_value_t = 800)]
    min_step_ms: u64,
    /// Timeout for each build and run phase in exec mode, in seconds.
    #[arg(long, default_value_t = 600)]
    step_timeout_secs: u64,
    /// Run each step inside this docker image instead of locally.
    #[arg(long)]
    docker_image: Option<String>,
}

const ASSETS: Assets = Assets {
    index_html: include_str!("../web/index.html"),
    app_js: include_str!("../web/app.js"),
    style_css: include_str!("../web/style.css"),
};

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mode = match args.mode {
        ModeArg::Replay => Mode::Replay,
        ModeArg::Exec => Mode::Exec,
    };
    let steps = relay::build_ring(&languages::get_languages(), &args.reference_dir, "ruby")?;
    eprintln!("ring of {} steps, mode {:?}", steps.len(), mode);
    let relay = Arc::new(Relay::new(steps, mode, args.reference_dir.clone()));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let config = RunConfig {
        mode,
        step_time: Duration::from_millis(args.step_ms),
        min_step_time: Duration::from_millis(args.min_step_ms),
        step_timeout: Duration::from_secs(args.step_timeout_secs),
        docker_image: args.docker_image.clone(),
        max_laps: None,
    };
    let runner = tokio::spawn(relay::run(relay.clone(), config, shutdown_rx.clone()));

    let app = server::router(relay, ASSETS, shutdown_rx);
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    eprintln!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            eprintln!("shutting down");
            let _ = shutdown_tx.send(true);
        })
        .await?;
    let _ = runner.await;
    Ok(())
}
