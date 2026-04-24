# Implementation Plan: btcfuse (Rust/Bitcoin)

---

## 1. Project Structure

```
btcfuse/
├── .github/
│   └── workflows/
│       └── ci.yml
├── Cargo.toml
├── Cargo.lock
├── src/
│   ├── main.rs                  ← entry point, CLI parsing, startup prefetch, mount + graceful unmount
│   ├── error.rs                 ← BtcFuseError enum, From<BtcFuseError> for libc::c_int
│   ├── fs/
│   │   ├── mod.rs
│   │   ├── root.rs              ← RootNode, top-level dispatch
│   │   ├── fetch.rs             ← fetch_cached<T,F> generic helper (negative→moka→sled→rpc)
│   │   ├── blocks.rs            ← BlocksDirNode, RecentDirNode, LatestDirNode, BlockNode, TxsDirNode, TxNode
│   │   ├── txs.rs               ← top-level TxsDirNode, TxNode (ghost)
│   │   ├── inputs.rs            ← InputsDirNode, InputNode
│   │   ├── outputs.rs           ← OutputsDirNode, OutputNode
│   │   ├── addresses.rs         ← AddressesDirNode, AddressNode
│   │   └── chain.rs             ← ChainNode, FeeDirNode
│   ├── client/
│   │   ├── mod.rs               ← BtcClient trait + shared types
│   │   ├── core.rs              ← Bitcoin Core JSON-RPC adapter (with rate limiter)
│   │   └── esplora.rs           ← optional Esplora REST adapter (with rate limiter)
│   └── cache/
│       └── mod.rs               ← moka (volatile+TTL) + sled/redb (finalized+persistent); get/set only
└── tests/
    └── integration.rs           ← integration tests (skipped without BTCFUSE_RPC_URL)
```

> Differences from a Go version: `inputs.rs`, `outputs.rs` are new (UTXO model). No ENS equivalent (no Bitcoin name service). `esplora.rs` is new — optional adapter for address indexing.

---

## 2. GitHub Issues Workflow

One issue per step, one PR per issue.

1. Finalize this plan
2. Create GitHub repo (`btcfuse`)
3. Create one issue per step below
4. Per issue: `feat/[issue-number]-[slug]` branch → implement → PR → merge → close

---

## 3. Build Steps

---

### Step 1 — Scaffold + CI

**Dependencies added:** none yet

```bash
cargo new btcfuse
```

Create directory structure with empty module declarations. No logic.

**CI pipeline** (`.github/workflows/ci.yml`):
- `cargo fmt --check`
- `cargo build`
- `cargo test`
- `cargo clippy -- -D warnings`
- `cargo audit`

**Cargo.toml** (workspace-level lints):
```toml
[workspace.lints.clippy]
pedantic = "warn"
```

---

### Step 2 — Error types + Bitcoin Core RPC client

**Dependencies added:**
```toml
[dependencies]
bitcoincore-rpc = "0.18"
rust-bitcoin = "0.32"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "1"
libc = "0.2"
governor = "0.6"
tracing = "0.1"
```

File: `src/error.rs` — define first, everything else depends on it:

```rust
#[derive(Debug, thiserror::Error)]
pub enum BtcFuseError {
    #[error("not found")]
    NotFound,
    #[error("invalid format: {0}")]
    InvalidFormat(String),
    #[error("RPC rate limit")]
    RateLimit,
    #[error("RPC error: {0}")]
    Rpc(String),
    #[error("cache error: {0}")]
    Cache(String),
}

impl From<BtcFuseError> for libc::c_int {
    fn from(e: BtcFuseError) -> Self {
        match e {
            BtcFuseError::NotFound         => libc::ENOENT,
            BtcFuseError::InvalidFormat(_) => libc::EINVAL,
            BtcFuseError::RateLimit        => libc::EAGAIN,
            BtcFuseError::Rpc(_)           => libc::EIO,
            BtcFuseError::Cache(_)         => libc::EIO,
        }
    }
}
```

Add `libc = "0.2"` to dependencies.

File: `src/client/mod.rs`

Define the trait — enables mock injection in tests:

```rust
pub trait BtcClient: Send + Sync {
    fn get_block_count(&self) -> Result<u64>;
    fn get_best_block_hash(&self) -> Result<BlockHash>;
    fn get_block_hash(&self, height: u64) -> Result<BlockHash>;
    fn get_block(&self, hash: &BlockHash) -> Result<Block>;
    fn get_raw_transaction(&self, txid: &Txid) -> Result<Transaction>;
    fn get_blockchain_info(&self) -> Result<BlockchainInfo>;
    fn estimate_smart_fee(&self, blocks: u16) -> Result<u64>;  // sat/vByte
    // Optional — only implemented by indexer-backed adapters:
    fn get_address_utxos(&self, addr: &Address) -> Result<Vec<Utxo>>;
}
```

Define value types shared across the codebase:

```rust
pub struct Block {
    pub hash: BlockHash,
    pub height: u64,
    pub previous_hash: BlockHash,
    pub timestamp: i64,
    pub merkle_root: String,
    pub nonce: u32,
    pub bits: String,
    pub difficulty: f64,
    pub size: u32,
    pub weight: u32,
    pub tx_ids: Vec<Txid>,
}

pub struct Transaction {
    pub txid: Txid,
    pub version: i32,
    pub size: u32,
    pub vsize: u32,
    pub weight: u32,
    pub locktime: u32,
    pub block_height: Option<u64>,
    pub confirmations: u64,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
}

pub struct Input {
    pub txid: Option<Txid>,   // None for coinbase
    pub vout: Option<u32>,    // None for coinbase
    pub coinbase: Option<String>, // hex; Some only for coinbase
    pub script: String,       // hex scriptSig
    pub value: Option<u64>,   // satoshis; resolved from previous output
}

pub struct Output {
    pub index: u32,
    pub value: u64,           // satoshis
    pub script: String,       // hex scriptPubKey
    pub address: Option<String>, // decoded; None if undecodable
}

pub struct Utxo {
    pub txid: Txid,
    pub vout: u32,
    pub value: u64,           // satoshis
    pub confirmations: u64,
}
```

Bitcoin Core JSON-RPC uses HTTP Basic Auth. Concrete implementation in `src/client/core.rs`:
- URL: `http://user:pass@host:port/`
- All calls via `bitcoincore-rpc` crate
- Rate limiting via `governor` token-bucket limiter (10 req/s, burst 5) — inside the concrete client, not the trait
- Startup validation: call `getblockchaininfo` in `CoreClient::new()` — fail fast if unreachable
- Error sanitization: strip `http://user:pass@...` credentials from error strings; return `BtcFuseError` variants, never raw strings

**Tests in this PR:**
- Retry on connection error and timeout
- Credentials never appear in returned error strings
- `get_block` maps JSON fields to `Block` struct correctly
- Rate limiter is called before each RPC attempt

---

### Step 3 — Cache layer

**Dependencies added:**
```toml
moka = { version = "0.12", features = ["sync"] }
sled = "0.34"            # or redb = "2"
bincode = "1"
```

File: `src/cache/mod.rs`

Dual-storage: `moka` for volatile TTL data, `sled`/`redb` for finalized persistent data. All values serialized with `bincode` — `Block` and `Transaction` must derive `serde::Serialize + serde::Deserialize`.

File: `src/fs/fetch.rs` — generic helper that implements the full lookup chain for all node `fetch_*` functions:

```rust
pub fn fetch_cached<T, F>(
    cache: &Cache,
    mem_key: &str,
    persist_bucket: &str,
    latest_height: u64,
    item_height: u64,
    rpc_fn: F,
) -> Result<T, BtcFuseError>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
    F: FnOnce() -> Result<T, BtcFuseError>,
```

This lives in `src/fs/` (not `src/cache/`) because finalization logic is an fs-layer concern. The `cache` module only provides get/set primitives.

TTL constants:

| Key pattern | TTL | Storage |
|-------------|-----|---------|
| `chain:height` | 30s | moka |
| `block:latest` | 60s | moka |
| `chain:fee` | 30s | moka |
| `address:{addr}` | 30s | moka |
| `negative:{path}` | 30s | moka |
| `tx:{txid}` (unconfirmed) | 60s | moka |
| `block:{hash}` (≥6 conf) | ∞ | sled |
| `tx:{txid}` (≥6 conf) | ∞ | sled |

**Tests:** TTL expiry, sled restart survival, negative cache hit prevents moka + sled reads, `fetch_cached` stores to sled when finalized.

---

### Step 4 — FUSE mount skeleton + config

**Dependencies added:**
```toml
fuser = "0.14"
clap = { version = "4", features = ["derive"] }
toml = "0.8"
```

File: `src/main.rs`

```rust
#[derive(Parser)]
struct Args {
    mountpoint: PathBuf,
    #[arg(long, env = "BTCFUSE_RPC_URL")]
    rpc_url: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let client: Arc<dyn BtcClient> = Arc::new(CoreClient::new(&args.rpc_url)?);
    let cache = Arc::new(Cache::new()?);

    // Pre-fetch startup data (network name) before mounting.
    // Node constructors must not make RPC calls.
    let info = client.get_blockchain_info()?;
    cache.set("chain:network", info.chain.as_bytes(), Duration::MAX);

    let fs = BtcFs::new(Arc::clone(&client), Arc::clone(&cache));
    let options = vec![MountOption::RO, MountOption::FSName("btcfuse".into())];

    // mount2 blocks until unmounted (Ctrl+C or fusermount -u)
    fuser::mount2(fs, &args.mountpoint, &options)?;
    Ok(())
}
```

Add `tracing-subscriber = "0.3"` to dev-dependencies (also used in production for logging).

Config file `~/.config/btcfuse/config.toml`:
```toml
rpc_url = "http://user:pass@localhost:8332"
network = "mainnet"
log_level = "info"

[indexer]
backend = "esplora"
esplora_url = "https://blockstream.info/api"
```

Config priority: CLI flag > `BTCFUSE_RPC_URL` env var > config file.

`BtcFs` returns empty root directory at this step.

---

### Step 5 — blocks/recent/ and blocks/[height]/

File: `src/fs/blocks.rs`

- `readdir` on `blocks/` → return last 10 block heights only
- `lookup("blocks", name)` → parse as u64 height; fetch `getblockhash(height)` then `getblock(hash)`; return `ENOENT` if not found
- `BlockNode` files: `info.json`, `hash`, `height`, `timestamp`, `merkle_root`

**Finalization check:** `latest_height - block_height >= 6` → sled; otherwise moka (TTL 60s).

Note: `getblock` with verbosity 1 returns txids only — full tx data is fetched lazily in Step 6.

**Tests in this PR:**
- `info.json` fields match mock RPC response
- Scalar files terminated with `\n`
- `ENOENT` for height beyond chain tip
- Finalized block (≥6 conf) stored in sled; recent block in moka

---

### Step 6 — blocks/[height]/txs/ with inputs/ and outputs/

Continue in `blocks.rs`, add `src/fs/inputs.rs` and `src/fs/outputs.rs`.

- `readdir` on `txs/` → list txids from cached block (txids already in Block struct, no extra RPC)
- `lookup(txid)` → fetch full tx via `getrawtransaction(txid, true)`; cache it
- `TxNode` files:
  - `info.json` — `{ txid, version, size, vsize, weight, locktime, fee, blockheight, confirmations }`
  - `fee` = sum(input values) − sum(output values) in satoshis; requires resolving inputs
- `InputsDirNode`: entries `0`, `1`, ..., `n-1`
- `InputNode` files: `txid`, `vout`, `value`, `script`
  - Coinbase: only `coinbase` file; `txid`/`vout`/`value` absent
- `OutputsDirNode`: entries `0`, `1`, ..., `n-1`
- `OutputNode` files: `value`, `script`, `address` (absent if non-standard script)

**Input value resolution:** to compute `fee` and populate `inputs/[n]/value`, the previous output's tx must be fetched. Strategy:
- Fetch lazily only when `value` file is read
- Cache resolved input values alongside the tx

**Tests in this PR:**
- `inputs/0/txid` and `inputs/0/vout` match raw tx data
- Coinbase tx: `inputs/0/coinbase` exists; `inputs/0/txid` returns `ENOENT`
- `outputs/0/value` is decimal satoshis terminated with `\n`
- `outputs/n/address` absent when script is OP_RETURN or non-standard

---

### Step 7 — blocks/latest/

Add to `blocks.rs`:

- `LatestDirNode` wraps latest block — **delegates to `BlockNode`**, does not duplicate lookup/readdir logic
- Pattern: resolve current tip height from cache (TTL 60s), construct a `BlockNode` with that height, and delegate all operations to it
- Source: `get_best_block_hash()` → `get_block(hash)`

**Tests in this PR:**
- After TTL expires, next read triggers RPC call
- Within TTL, no extra RPC calls made
- `LatestDirNode.lookup(name)` returns same result as `BlockNode::new(latest_height).lookup(name)`

---

### Step 8 — txs/[txid]/ (top-level, ghost)

File: `src/fs/txs.rs`

- `readdir` on root `txs/` → empty (no `ls` at root `txs/`)
- `lookup(txid)` → validate 64-char hex; `EINVAL` if malformed; `ENOENT` if not found
- Same `TxNode`, `InputsDirNode`, `OutputsDirNode` as Step 6

Txid validation: must be exactly 64 lowercase hex chars. Use `Txid::from_str` from `rust-bitcoin`.

**Tests in this PR:**
- `readdir` on root `txs/` returns empty list
- Known txid resolves correctly (same node structure as block-nested tx)
- 63-char input returns `EINVAL`
- Unknown txid returns `ENOENT`

---

### Step 9 — addresses/[address]/

File: `src/fs/addresses.rs`

- `lookup(addr)` → validate via `Address::from_str` from `rust-bitcoin`; `EINVAL` if malformed
- `AddressNode` files — TTL 30s:
  - `balance` — sum of UTXO values in satoshis, decimal integer
  - `utxos.json` — `[{ txid, vout, value, confirmations }]`
- If `get_address_utxos()` is not configured (no indexer): return `ENOENT` for all address lookups with a logged warning

Address format support (via `rust-bitcoin`):
- P2PKH: `1...` (legacy)
- P2SH: `3...` (script hash)
- P2WPKH / P2WSH: `bc1q...` (native SegWit)
- P2TR: `bc1p...` (Taproot)

**Tests in this PR:**
- P2PKH, P2WPKH, P2TR addresses all decode without `EINVAL`
- `balance` = sum of mock UTXO values
- `utxos.json` contains correct entries
- Malformed address returns `EINVAL`
- No indexer configured: lookup returns `ENOENT` + log warning emitted

---

### Step 10 — Esplora adapter (optional indexer)

**Dependencies added:**
```toml
reqwest = { version = "0.12", features = ["blocking", "json"] }
```

File: `src/client/esplora.rs`

Implements `BtcClient::get_address_utxos()` via Esplora REST API:

```
GET https://blockstream.info/api/address/{addr}/utxo
→ [{ txid, vout, status: { confirmed, block_height }, value }]
```

REST adapter, not JSON-RPC. Implements the same `BtcClient` trait.

Rate limiting: use `governor` token bucket (same as `CoreClient`) — Blockstream's public Esplora has a ~1 req/s soft limit.

**Tests in this PR:**
- Mock HTTP server returns Esplora-format JSON; assert correct `Utxo` structs parsed
- HTTP 429 → retry with backoff

---

### Step 11 — chain/ and chain/fee/

File: `src/fs/chain.rs`

- `ChainNode` files:
  - `height` — refreshed every 30s via background thread (`get_block_count`)
  - `hash` — refreshed with `height` (`get_best_block_hash`)
  - `network` — fetched once at startup in `main.rs`, stored in cache before mounting; **not fetched in `ChainNode::new()`**
  - `difficulty` — refreshed with `height`

> **No RPC calls in constructors.** `ChainNode::new()` only receives `Arc<Cache>` and `Arc<dyn BtcClient>`. All one-time startup data (`network`) is fetched in `main.rs` and pre-populated into the cache before `fuser::mount2` is called.
- `FeeDirNode` files: `slow`, `standard`, `fast` — sat/vByte, TTL 30s
  - `slow`:     `estimate_smart_fee(6)` — targets 6 blocks (~1 hour)
  - `standard`: `estimate_smart_fee(3)` — targets 3 blocks (~30 min)
  - `fast`:     `estimate_smart_fee(1)` — targets 1 block (~10 min)

Fee conversion: `estimate_smart_fee` returns BTC/kB → `sat_per_vbyte = (btc_per_kb * 1e8 / 1000).ceil() as u64`.

**Tests in this PR:**
- `network` fetched once — zero extra RPC calls on repeated reads
- `height` increments after background refresh
- Fee conversion: `btc_per_kb * 1e8 / 1000` produces correct sat/vByte integer

---

### Step 12 — Integration tests

```bash
BTCFUSE_RPC_URL=http://user:pass@localhost:18332 cargo test --test integration
```

Set `BTCFUSE_RPC_URL` to a Bitcoin Core JSON-RPC endpoint. Tests skip if env var is not set.

RPC endpoint options:
- **Bitcoin Core** on testnet4/signet — self-hosted, no rate limits, primary target
- **QuickNode** — managed Bitcoin node provider; JSON-RPC compatible with Bitcoin Core; use when self-hosted node is not available (equivalent of Alchemy for Ethereum)

Alchemy is **not** compatible — it only supports Ethereum/EVM chains.

| Scenario | What to verify |
|----------|---------------|
| `cat chain/height` | decimal integer, increments over time |
| `cat blocks/{height}/info.json` | valid JSON, matches known block |
| `ls blocks/{height}/txs/` | correct tx count matches `ntx` in block |
| `cat txs/{txid}/outputs/0/value` | satoshis match known transaction |
| `cat txs/{coinbase_txid}/inputs/0/coinbase` | file present; `txid` file absent |
| `cat addresses/{addr}/balance` | satoshis match UTXO sum from indexer |
| Write attempt | returns `EROFS` |
| Invalid txid (63 chars) | returns `EINVAL` |
| Ghost block accessible by height | accessible by path, not in `ls blocks/recent/` |

---

## 4. Manual Testing

```bash
# 1. Build
cargo build --release

# 2. Mount (Bitcoin Core on localhost)
mkdir -p /tmp/btcfuse
./target/release/btcfuse /tmp/btcfuse --rpc-url "http://user:pass@localhost:18332"

# 3. Explore
cat /tmp/btcfuse/chain/height
cat /tmp/btcfuse/chain/network
cat /tmp/btcfuse/chain/fee/fast
cat /tmp/btcfuse/blocks/840000/info.json
ls /tmp/btcfuse/blocks/840000/txs/
cat /tmp/btcfuse/txs/{txid}/info.json
ls /tmp/btcfuse/txs/{txid}/inputs/
cat /tmp/btcfuse/txs/{txid}/inputs/0/value
cat /tmp/btcfuse/addresses/{addr}/balance

# 4. Unmount
umount /tmp/btcfuse           # Linux
diskutil unmount /tmp/btcfuse  # macOS
```

---

## 5. Security Notes

- RPC credentials (`http://user:pass@host:port`) never appear in filesystem paths or file contents
- Config file stored with `chmod 600`
- Filesystem is read-only — all writes return `EROFS`
- No private key operations — wallet functionality is entirely out of scope

---

## 6. Future Versions

### v2

- **Mempool:** `mempool/size`, `mempool/bytes`, `mempool/txs/[txid]/` — via `getmempoolinfo` + `getrawmempool`; never cached
- **OP_RETURN data:** `txs/[txid]/outputs/[n]/op_return` — decode when `scriptPubKey` starts with `OP_RETURN`
- **Coinbase message:** `blocks/[height]/coinbase_msg` — decode ASCII from coinbase input scriptSig
- **Taproot witness:** `txs/[txid]/inputs/[n]/witness` — hex witness data for SegWit/Taproot inputs

### v3

- **Address tx history:** `addresses/[addr]/txs/` — symlinks to `txs/[txid]/`; requires Esplora `GET /address/{addr}/txs`
- **Multi-sig info:** `txs/[txid]/outputs/[n]/multisig.json` — decoded m-of-n if P2MS or P2SH-multisig

### Out of Scope

- **Wallet operations** — private keys, signing, broadcasting
- **Lightning Network channels** — different protocol layer
- **Inscriptions / Ordinals** — could be v3 if there's demand: `txs/[txid]/outputs/[n]/inscription`
