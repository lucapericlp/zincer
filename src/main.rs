mod args;
mod build_api;

use std::path::Path;

use args::{MusicPlatformDst, RootArgs};
use build_api::BuildApi;
use clap::Parser;
use color_eyre::eyre::{Result, eyre};
use serde_json::json;
use sync_dis_boi::events;
use sync_dis_boi::export::export;
use sync_dis_boi::import::import;
use sync_dis_boi::sync::synchronize;
use tracing::{Level, debug, info};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    // CLI arguments
    let args = RootArgs::parse();

    // Setup logging
    const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
    let level: Level = args.logging.clone().into();
    let filter = Targets::new().with_target(CRATE_NAME, Level::TRACE);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(true)
        .without_time()
        .finish()
        .with(filter)
        .init();
    debug!("logging level: {}", level);
    events::enable(args.config.progress_events);

    let config_dir = dirs::config_dir().ok_or(eyre!("couldn't find system config dir"))?;
    let config_dir = config_dir.join("SyncDisBoi");
    if !config_dir.exists() {
        info!("creating SyncDisBoi config directory: {:?}", config_dir);
        std::fs::create_dir_all(&config_dir)?;
    }

    if args.config.debug {
        let debug_dir = Path::new("debug");
        if !debug_dir.exists() {
            debug!("creating debug directory");
            std::fs::create_dir("debug")?;
        }
    }

    match args.src.get_dst() {
        MusicPlatformDst::Export { output, minify } => {
            let src_api = args.src.parse(&args, &config_dir).await?;
            export(src_api, output, *minify).await?;
        }
        MusicPlatformDst::Import { input } => {
            let src_api = args.src.parse(&args, &config_dir).await?;
            import(input, src_api, args.config).await?;
        }
        MusicPlatformDst::Playlists => {
            let src_api = args.src.parse(&args, &config_dir).await?;
            let playlists = src_api.get_playlists_info().await?;
            let listing: Vec<_> = playlists
                .iter()
                .map(|p| json!({"id": p.id, "name": p.name}))
                .collect();
            println!("{}", serde_json::to_string(&listing)?);
        }
        _ => {
            events::emit("stage", json!({"stage": "auth_destination"}));
            let dst_api = args.src.get_dst().parse(&args, &config_dir).await?;
            events::emit("stage", json!({"stage": "auth_source"}));
            let src_api = args.src.parse(&args, &config_dir).await?;
            synchronize(src_api, dst_api, args.config).await?;
        }
    }

    Ok(())
}
