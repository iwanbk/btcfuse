mod cache;
mod client;
mod error;
mod fs;

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use fuser::MountOption;
use serde::Deserialize;
use tracing::info;

use cache::Cache;
use client::core::CoreClient;
use client::BtcClient;
use error::BtcFuseError;
use fs::BtcFs;

#[derive(Parser)]
#[command(about = "Mount the Bitcoin blockchain as a FUSE filesystem")]
struct Args {
    mountpoint: PathBuf,
    /// RPC URL (http://user:pass@host:port). Overrides env var and config file.
    #[arg(long, env = "BTCFUSE_RPC_URL")]
    rpc_url: Option<String>,
}

#[derive(Deserialize, Default)]
struct Config {
    rpc_url: Option<String>,
    #[allow(dead_code)]
    log_level: Option<String>,
}

fn load_config() -> Config {
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

fn config_path() -> PathBuf {
    env::var("HOME").map_or_else(
        |_| PathBuf::from("config.toml"),
        |home| {
            Path::new(&home)
                .join(".config")
                .join("btcfuse")
                .join("config.toml")
        },
    )
}

fn sled_path() -> PathBuf {
    env::var("HOME").map_or_else(
        |_| PathBuf::from(".btcfuse-sled"),
        |home| Path::new(&home).join(".cache").join("btcfuse").join("sled"),
    )
}

/// Splits `http://user:pass@host:port` into `(url_without_creds, user, pass)`.
fn parse_rpc_url(raw: &str) -> Result<(String, String, String), BtcFuseError> {
    let after_scheme = raw.find("://").map_or(0, |i| i + 3);
    if let Some(at_offset) = raw[after_scheme..].find('@') {
        let at = after_scheme + at_offset;
        let creds = &raw[after_scheme..at];
        let colon = creds
            .find(':')
            .ok_or_else(|| BtcFuseError::InvalidFormat("missing ':' in RPC credentials".into()))?;
        let user = creds[..colon].to_owned();
        let pass = creds[colon + 1..].to_owned();
        let url = format!("{}{}", &raw[..after_scheme], &raw[at + 1..]);
        Ok((url, user, pass))
    } else {
        Ok((raw.to_owned(), String::new(), String::new()))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let config = load_config();

    let rpc_url = args
        .rpc_url
        .or(config.rpc_url)
        .ok_or("RPC URL required: use --rpc-url, BTCFUSE_RPC_URL env var, or config file")?;

    let (url, user, pass) = parse_rpc_url(&rpc_url)?;
    let client: Arc<dyn BtcClient> = Arc::new(CoreClient::new(&url, &user, &pass)?);
    let cache = Arc::new(Cache::new(&sled_path())?);

    let info = client.get_blockchain_info()?;
    cache.set("chain:network", info.chain.as_bytes(), Duration::MAX);
    info!(chain = %info.chain, height = info.blocks, "connected to Bitcoin node");

    let btcfs = BtcFs::new(Arc::clone(&client), Arc::clone(&cache));
    let options = vec![MountOption::RO, MountOption::FSName("btcfuse".into())];
    fuser::mount2(btcfs, &args.mountpoint, &options)?;
    Ok(())
}
