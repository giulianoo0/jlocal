//! jlocal — loopback API + tiny status/version window.

mod api;
mod status;
mod ui;

use std::net::{Ipv4Addr, SocketAddr};

use clap::Parser;

/// jlocal — local companion for juntos.lol
#[derive(Parser, Debug)]
#[command(name = "jlocal", version = status::VERSION)]
struct Args {
    /// Loopback port for the local API (browser probes this).
    #[arg(long, env = "JLOCAL_PORT", default_value_t = status::DEFAULT_PORT)]
    port: u16,

    /// Run headless: no window, just the loopback API + console status.
    #[arg(long, env = "JLOCAL_NO_UI")]
    no_ui: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "jlocal=info".into()),
        )
        .init();

    let args = Args::parse();
    let state = status::AppState::new();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, args.port));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Bind BEFORE opening any window so "already running" is a clear error,
    // never a silent port-hop (the web UI probes one fixed port).
    let listener = match rt.block_on(tokio::net::TcpListener::bind(addr)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "jlocal {} — status: NOT connected ({addr} unavailable: {e}). \
                 Is another copy already running?",
                status::VERSION
            );
            std::process::exit(1);
        }
    };

    println!(
        "jlocal {} — status: connected (http://127.0.0.1:{})",
        status::VERSION,
        args.port
    );
    rt.spawn(api::serve(listener, state.clone()));

    if args.no_ui {
        rt.block_on(async {
            let _ = tokio::signal::ctrl_c().await;
        });
        println!("jlocal {} — status: stopped", status::VERSION);
    } else {
        ui::run(rt, state, args.port);
    }
    Ok(())
}
