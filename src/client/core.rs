use std::num::NonZeroU32;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bitcoin::hex::DisplayHex;
use bitcoin::{BlockHash, Txid};
use bitcoincore_rpc::jsonrpc;
use bitcoincore_rpc::{Auth, Client, RpcApi};
use governor::clock::{Clock, DefaultClock};
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use tracing::{debug, warn};

use super::{Block, BlockchainInfo, BtcClient, Input, Output, Transaction};
use crate::error::BtcFuseError;

const MAX_ATTEMPTS: u32 = 3;
const BASE_BACKOFF_MS: u64 = 250;
const MAX_BACKOFF_MS: u64 = 10_000;
const RATE_LIMIT_RPS: u32 = 10;
const RATE_LIMIT_BURST: u32 = 5;

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

pub struct CoreClient {
    inner: Client,
    limiter: Arc<Limiter>,
    url: String,
}

impl CoreClient {
    pub fn new(url: &str, user: &str, pass: &str) -> Result<Self, BtcFuseError> {
        let client = Self::build(url, user, pass)?;
        // Fail fast: validate connectivity.
        client.get_blockchain_info()?;
        Ok(client)
    }

    fn build(url: &str, user: &str, pass: &str) -> Result<Self, BtcFuseError> {
        let inner = Client::new(url, Auth::UserPass(user.to_owned(), pass.to_owned()))
            .map_err(|e| BtcFuseError::Rpc(sanitize(url, &e.to_string())))?;
        let quota = Quota::per_second(NonZeroU32::new(RATE_LIMIT_RPS).unwrap())
            .allow_burst(NonZeroU32::new(RATE_LIMIT_BURST).unwrap());
        let limiter = Arc::new(RateLimiter::direct(quota));
        Ok(Self {
            inner,
            limiter,
            url: url.to_owned(),
        })
    }

    fn wait_for_rate(&self) {
        let clock = DefaultClock::default();
        loop {
            match self.limiter.check() {
                Ok(()) => return,
                Err(not_until) => {
                    let wait = not_until.wait_time_from(clock.now());
                    thread::sleep(wait);
                }
            }
        }
    }

    fn call<T, F>(&self, f: F) -> Result<T, BtcFuseError>
    where
        F: Fn() -> Result<T, bitcoincore_rpc::Error>,
    {
        let mut delay_ms = BASE_BACKOFF_MS;
        for attempt in 0..MAX_ATTEMPTS {
            self.wait_for_rate();
            match f() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let msg = sanitize(&self.url, &e.to_string());
                    if attempt + 1 < MAX_ATTEMPTS && is_transient(&e) {
                        warn!(
                            "RPC attempt {}/{MAX_ATTEMPTS} failed: {msg}; retrying",
                            attempt + 1
                        );
                        thread::sleep(Duration::from_millis(delay_ms));
                        delay_ms = (delay_ms * 2).min(MAX_BACKOFF_MS);
                    } else {
                        return Err(map_rpc_error(&e, msg));
                    }
                }
            }
        }
        unreachable!()
    }
}

fn sanitize(url: &str, msg: &str) -> String {
    // Strip http(s)://user:pass@host from error strings to prevent credential leaks.
    if let Some(at) = url.find('@') {
        let scheme_end = url.find("://").map_or(0, |i| i + 3);
        let credentials = &url[scheme_end..=at];
        let scheme = &url[..scheme_end];
        msg.replace(
            &format!("{scheme}{credentials}"),
            &format!("{scheme}<redacted>@"),
        )
        .replace(url, "<redacted-url>")
    } else {
        msg.to_owned()
    }
}

fn is_transient(e: &bitcoincore_rpc::Error) -> bool {
    use bitcoincore_rpc::Error;
    matches!(e, Error::JsonRpc(jsonrpc::Error::Transport(_)))
}

fn map_rpc_error(e: &bitcoincore_rpc::Error, msg: String) -> BtcFuseError {
    use bitcoincore_rpc::Error;
    match e {
        Error::JsonRpc(jsonrpc::Error::Rpc(rpc)) if rpc.code == -5 || rpc.code == -8 => {
            BtcFuseError::NotFound(msg)
        }
        _ => BtcFuseError::Rpc(msg),
    }
}

impl BtcClient for CoreClient {
    fn get_blockchain_info(&self) -> Result<BlockchainInfo, BtcFuseError> {
        let info = self.call(|| self.inner.get_blockchain_info())?;
        Ok(BlockchainInfo {
            chain: info.chain.to_string(),
            blocks: info.blocks,
            best_block_hash: info.best_block_hash,
        })
    }

    fn get_block_count(&self) -> Result<u64, BtcFuseError> {
        self.call(|| self.inner.get_block_count())
    }

    fn get_block_hash(&self, height: u64) -> Result<BlockHash, BtcFuseError> {
        self.call(|| self.inner.get_block_hash(height))
    }

    fn get_block(&self, hash: &BlockHash) -> Result<Block, BtcFuseError> {
        let info = self.call(|| self.inner.get_block_info(hash))?;
        debug!(hash = %hash, height = info.height, "fetched block");
        Ok(Block {
            hash: info.hash,
            height: info.height as u64,
            time: info.time as u64,
            tx: info.tx,
            confirmations: i64::from(info.confirmations),
        })
    }

    fn get_transaction(&self, txid: &Txid) -> Result<Transaction, BtcFuseError> {
        let raw = self.call(|| self.inner.get_raw_transaction_info(txid, None))?;

        let inputs = raw
            .vin
            .into_iter()
            .map(|vin| Input {
                txid: vin.txid,
                vout: vin.vout,
                value: None,
                coinbase: vin.coinbase.map(|b| format!("{}", b.as_hex())),
            })
            .collect();

        let outputs = raw
            .vout
            .into_iter()
            .map(|vout| {
                let address = vout
                    .script_pub_key
                    .address
                    .or_else(|| vout.script_pub_key.addresses.into_iter().next())
                    .map(|a| a.assume_checked().to_string());
                Output {
                    value: vout.value.to_sat(),
                    script_pubkey: format!("{}", vout.script_pub_key.hex.as_hex()),
                    address,
                }
            })
            .collect();

        Ok(Transaction {
            txid: raw.txid,
            block_hash: raw.blockhash,
            confirmations: raw.confirmations.map_or(0, i64::from),
            inputs,
            outputs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK_HASH: &str = "000000000000000000025948cc4e7f62d2ba31a3edf8bcce40c9dc4e79bc7abe";
    const TXID: &str = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";

    fn rpc_ok(result: serde_json::Value) -> String {
        serde_json::json!({ "id": 1, "result": result, "error": null }).to_string()
    }

    fn rpc_err_body(code: i32, msg: &str) -> String {
        serde_json::json!({ "id": 1, "result": null, "error": { "code": code, "message": msg } })
            .to_string()
    }

    // getnetworkinfo response (needed by version() inside get_blockchain_info).
    fn network_info_body() -> String {
        rpc_ok(serde_json::json!({ "version": 230_000u64 }))
    }

    fn blockchain_info_body() -> String {
        rpc_ok(serde_json::json!({
            "chain": "main",
            "blocks": 840_000u64,
            "headers": 840_000u64,
            "bestblockhash": BLOCK_HASH,
            "difficulty": 83_148_355_189_239.27_f64,
            "mediantime": 1_713_571_522u64,
            "verificationprogress": 0.9999,
            "initialblockdownload": false,
            "chainwork": "00000000000000000000000000000000000000007e564c65d0a0b8b29254e92e",
            "size_on_disk": 0u64,
            "pruned": false,
            "softforks": {},
            "warnings": ""
        }))
    }

    fn block_body() -> String {
        // get_block_info() calls "getblock" (verbosity=1) — not "getblockinfo".
        rpc_ok(serde_json::json!({
            "hash": BLOCK_HASH,
            "confirmations": 12,
            "size": 285,
            "weight": 1140,
            "height": 840_000u64,
            "version": 1i32,
            "merkleroot": TXID,
            "tx": [TXID],
            "time": 1_231_006_505usize,
            "nonce": 2_083_236_893u32,
            "bits": "1d00ffff",
            "difficulty": 1.0_f64,
            "chainwork": "0000000000000000000000000000000000000000000000000000000100010001",
            "nTx": 1usize,
            "previousblockhash": serde_json::Value::Null,
            "nextblockhash": serde_json::Value::Null
        }))
    }

    // One-shot mock: any POST / → 200 with given body.
    fn once(server: &mut mockito::Server, body: String) -> mockito::Mock {
        server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .expect(1)
            .create()
    }

    // build() skips startup validation — use for tests that don't need it.
    fn build_client(server: &mockito::Server) -> CoreClient {
        CoreClient::build(&server.url(), "user", "pass").expect("build failed")
    }

    // ── sanitize() unit tests (no HTTP needed) ───────────────────────────────

    #[test]
    fn test_credentials_stripped_from_errors() {
        let url = "http://alice:s3cr3t@127.0.0.1:8332";
        let cleaned = sanitize(url, &format!("connect to {url} timed out"));
        assert!(!cleaned.contains("s3cr3t"), "password leaked: {cleaned}");
        assert!(
            !cleaned.contains("alice:s3cr3t"),
            "credentials leaked: {cleaned}"
        );
        assert!(cleaned.contains("http://"), "scheme must remain");
    }

    #[test]
    fn test_no_credentials_unchanged() {
        assert_eq!(
            sanitize("http://127.0.0.1:8332", "connection refused"),
            "connection refused"
        );
    }

    // ── rate limiter ─────────────────────────────────────────────────────────

    #[test]
    fn test_rate_limiter_consulted() {
        let server = mockito::Server::new();
        let client = build_client(&server);
        // Burst capacity = 5, so wait_for_rate() returns immediately on a fresh limiter.
        client.wait_for_rate();
    }

    // ── get_block field mapping ───────────────────────────────────────────────

    #[test]
    fn test_get_block_maps_fields() {
        let mut server = mockito::Server::new();
        // get_block_info() calls RPC method "getblock" with verbosity=1.
        let _m = once(&mut server, block_body());

        let client = build_client(&server);
        let hash: BlockHash = BLOCK_HASH.parse().unwrap();
        let block = client.get_block(&hash).expect("get_block failed");

        assert_eq!(block.height, 840_000);
        assert_eq!(block.time, 1_231_006_505);
        assert_eq!(block.confirmations, 12);
        assert_eq!(block.tx.len(), 1);
        assert_eq!(block.tx[0].to_string(), TXID);
        assert_eq!(block.hash.to_string(), BLOCK_HASH);
    }

    // ── error mapping ─────────────────────────────────────────────────────────

    #[test]
    fn test_not_found_rpc_error_maps_to_not_found() {
        let mut server = mockito::Server::new();
        let _m = once(
            &mut server,
            rpc_err_body(-5, "No information available about transaction"),
        );

        let client = build_client(&server);
        let txid: Txid = TXID.parse().unwrap();
        let err = client.get_transaction(&txid).unwrap_err();
        assert!(
            matches!(err, BtcFuseError::NotFound(_)),
            "expected NotFound, got {err}"
        );
    }

    // ── startup validation ────────────────────────────────────────────────────
    // get_blockchain_info() calls version() → getnetworkinfo first, then getblockchaininfo.
    // Mocks are consumed FIFO by expect(1), so register networkinfo mock first.

    #[test]
    fn test_startup_validates_connectivity() {
        let mut server = mockito::Server::new();
        // get_blockchain_info() calls getblockchaininfo first, then getnetworkinfo (version check).
        let _m1 = once(&mut server, blockchain_info_body());
        let _m2 = once(&mut server, network_info_body());
        let url = server.url();
        assert!(CoreClient::new(&url, "user", "pass").is_ok());
    }

    #[test]
    fn test_startup_fails_on_rpc_error() {
        let mut server = mockito::Server::new();
        // version() fails → startup aborts immediately.
        let _m = once(&mut server, rpc_err_body(-28, "Loading block index..."));
        let url = server.url();
        assert!(CoreClient::new(&url, "user", "pass").is_err());
    }

    // ── retry ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_retry_succeeds_after_transient_failures() {
        let mut server = mockito::Server::new();
        // get_block_count() → single "getblockcount" RPC call (no version check).
        // Two 503s → transport errors → retry; third attempt → 200 with result.
        let _m_fail1 = server.mock("POST", "/").with_status(503).expect(1).create();
        let _m_fail2 = server.mock("POST", "/").with_status(503).expect(1).create();
        let _m_ok = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(rpc_ok(serde_json::json!(840_000u64)))
            .expect(1)
            .create();

        let client = build_client(&server);
        let result = client.get_block_count();
        assert!(result.is_ok(), "expected retry to succeed: {result:?}");
    }
}
