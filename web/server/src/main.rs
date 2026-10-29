//! vibery-web: HTTP server over the vibery engine. Contract: ../API.md
#![allow(dead_code)]

#[path = "../../../src/catalog.rs"]
mod catalog;
#[path = "../../../src/engine.rs"]
mod engine;
#[path = "../../../src/index.rs"]
mod index;
#[path = "../../../src/ingest.rs"]
mod ingest;
#[path = "../../../src/query.rs"]
mod query;
#[path = "../../../src/store.rs"]
mod store;
#[path = "../../../src/text.rs"]
mod text;
#[path = "../../../src/util.rs"]
mod util;

mod rows;
mod api;

use clap::Parser as _;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(clap::Parser)]
struct Cli {
    #[arg(long, default_value = "/Users/tbirch/src/neara/fibery/vibery/index")]
    index: PathBuf,
    #[arg(long, default_value = "/Users/tbirch/src/neara/fibery")]
    data: PathBuf,
    #[arg(long, default_value = "/Users/tbirch/src/neara/fibery/vibery/web")]
    web: PathBuf,
    #[arg(long, default_value = "127.0.0.1:7878")]
    addr: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let t0 = std::time::Instant::now();
    let engine = engine::Engine::open(&cli.index)?;
    eprintln!("engine open {:.2}s", t0.elapsed().as_secs_f64());
    let rows = rows::Rows::load_or_build(&cli.data, &cli.web.join("cache/offsets.bin"), &engine.catalog)?;
    eprintln!("rows ready {:.2}s ({} rows)", t0.elapsed().as_secs_f64(), rows.offs.len());
    let state = Arc::new(api::AppState::new(engine, rows, cli.data.clone(), cli.web.clone())?);
    state.warm();
    eprintln!("warm {:.2}s", t0.elapsed().as_secs_f64());
    let app = api::router(state, &cli.web.join("ui/dist"));
    let listener = tokio::net::TcpListener::bind(&cli.addr).await?;
    eprintln!("listening on http://{} ({:.2}s)", cli.addr, t0.elapsed().as_secs_f64());
    axum::serve(listener, app).await?;
    Ok(())
}
