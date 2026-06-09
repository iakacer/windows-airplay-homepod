//! homepod-cast — stream Windows system audio to a HomePod over AirPlay 2.
//!
//! Run with no arguments to launch the system-tray app. Run `--list` to print
//! discovered AirPlay devices and exit.

// Use the Windows subsystem (no console window) for the normal tray app, but
// keep a console when built for debugging via `--list`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cast;
mod tray;

use std::time::Duration;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "warn,homepod_cast=info".into()),
        ))
        .init();

    let args: Vec<String> = std::env::args().collect();

    // Diagnostic: reproduce start -> stream -> stop -> restart -> stream with logs.
    if args.iter().any(|a| a == "--selftest") {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let devices = match cast::discover(Duration::from_secs(3)).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("discover failed: {e:#}");
                    return;
                }
            };
            let Some(dev) = devices.into_iter().next() else {
                tracing::error!("no device found");
                return;
            };
            tracing::info!("selftest target: {} ({})", dev.name, dev.model);
            tracing::info!("=== starting session ===");
            match cast::Session::start(dev.clone(), cast::DEFAULT_VOLUME).await {
                Ok(mut s) => {
                    let secs = 50u32;
                    tracing::info!("streaming for {secs}s with 2s keepalive (play audio now)");
                    let mut elapsed = 0;
                    while elapsed < secs {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        s.feedback().await;
                        elapsed += 2;
                    }
                    s.stop().await;
                    tracing::info!("stopped");
                }
                Err(e) => tracing::error!("start failed: {e:#}"),
            }
            tracing::info!("selftest complete");
        });
        // The library leaks an infinite spawn_blocking task; force shutdown so
        // we don't hang on runtime drop.
        rt.shutdown_timeout(Duration::from_millis(300));
        return Ok(());
    }

    if args.iter().any(|a| a == "--list") {
        let rt = tokio::runtime::Runtime::new()?;
        let devices = rt.block_on(cast::discover(Duration::from_secs(3)))?;
        if devices.is_empty() {
            println!("No AirPlay devices found.");
        } else {
            println!("AirPlay devices:");
            for d in &devices {
                let ipv4 = d.addresses.iter().find(|a| a.is_ipv4());
                println!(
                    "  {:<24} {:<18} {}",
                    d.name,
                    d.model,
                    ipv4.map(|a| a.to_string()).unwrap_or_default()
                );
            }
        }
        return Ok(());
    }

    tray::run()
}
