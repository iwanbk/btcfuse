# Architecture: btcfuse (Rust/Bitcoin)

---

## 0. Language Decision: Go vs Rust

**Recommendation: Rust.**

| Dimension | Go | Rust |
|-----------|-----|------|
| FUSE library | `hanwen/go-fuse/v2` — battle-tested | `fuser` crate — solid, trait-based API |
| Bitcoin library | `btcsuite/btcd` — mature but development slowing | `rust-bitcoin` — canonical, very active; used by LDK, BDK, Fedimint |
| Bitcoin ecosystem | Smaller, fewer modern projects | Dominant — most new Bitcoin tooling (Lightning, wallets) is in Rust |
| Memory safety | GC adds latency on large caches | GC-free, better for long-running daemons |

**Why Rust:** The `rust-bitcoin` ecosystem is where active Bitcoin development happens — LDK, BDK, Fedimint, and most modern tooling are all Rust. The `fuser` + `rust-bitcoin` + `bitcoincore-rpc` stack is mature and well-maintained. For a long-running FUSE daemon, Rust's absence of GC pauses is a better fit.

**Why not Go:** `btcd` is functional but the Go Bitcoin ecosystem has less momentum. Go's advantage is familiarity, not fit.

---

## 1. Overview

### Bitcoin Data Model

Bitcoin's data model drives all the key design decisions.

| Aspect | Detail |
|--------|--------|
| Data model | UTXO-based — no accounts, only unspent outputs |
| Transaction shape | `inputs[]` + `outputs[]` (UTXO graph) |
| Address balance | Sum of UTXOs — requires indexer |
| Smart contracts | None |
| Name resolution | None |
| Fee unit | satoshis / virtual byte (sat/vByte) |
| Block time | ~10 minutes |
| Finality threshold | 6 confirmations (~1 hour) |
| RPC protocol | Bitcoin Core JSON-RPC |
| Language / library | Rust — `rust-bitcoin` + `bitcoincore-rpc` |

### UTXO Model Impact

In Ethereum, `eth_getBalance(address)` is a single O(1) RPC call. In Bitcoin, an address's balance is the sum of its unspent transaction outputs (UTXOs). Standard Bitcoin Core JSON-RPC does not provide this in one call — it requires either:

- Bitcoin Core with `-txindex` + `-addressindex` flags (non-default)
- An external indexer (Electrum server, Esplora, Blockstream API)

**Design decision:** btcfuse uses a pluggable backend interface. The primary backend is Bitcoin Core JSON-RPC (for self-hosted nodes). An optional Esplora adapter provides address/UTXO lookup for users without a full indexed node.

### Workflow Coverage

| Workflow | Mempool.space equivalent | btcfuse | Status |
|----------|--------------------------|---------|--------|
| View latest block height | Homepage | `cat chain/height` | v1 |
| Browse recent blocks | "Latest Blocks" list | `ls blocks/recent/` | v1 |
| Block detail by number | `/block/{hash}` | `cat blocks/840000/info.json` | v1 |
| List transactions in a block | Block → Transactions | `ls blocks/840000/txs/` | v1 |
| Transaction detail | `/tx/{txid}` | `cat txs/{txid}/info.json` | v1 |
| Transaction inputs | TX → Inputs | `ls txs/{txid}/inputs/` | v1 |
| Transaction outputs | TX → Outputs | `ls txs/{txid}/outputs/` | v1 |
| Address balance | `/address/{addr}` | `cat addresses/{addr}/balance` | v1 (requires indexer) |
| Address UTXOs | Address → UTXO tab | `cat addresses/{addr}/utxos.json` | v1 (requires indexer) |
| Fee rate tracker | Fee estimates | `cat chain/fee/fast` | v1 |
| Chain info | Network selector | `cat chain/network`, `cat chain/height` | v1 |
| BTC price (USD/EUR) | — | `cat chain/price/usd` | v1 (optional, requires price API) |
| Mempool size | Mempool stats | `cat mempool/size` | v2 |
| Mempool transactions | Mempool explorer | `ls mempool/txs/` | v2 |
| Address tx history | Address → Transactions | `ls addresses/{addr}/txs/` | v3 (indexer) |
| OP_RETURN data | TX → OP_RETURN | `cat txs/{txid}/outputs/[n]/op_return` | v2 |

---

## 2. Filesystem Design

### Layer Architecture

```
┌─────────────────────────────────────┐
│           CLI (clap)                │
│   btcfuse mount /mnt/btc --rpc=...    │
└──────────────────┬──────────────────┘
                   │ mount()
┌──────────────────▼──────────────────┐
│        FUSE Layer (fuser)           │
│   intercepts: lookup, getattr,      │
│   readdir, read                     │
└──────────────────┬──────────────────┘
                   │
┌──────────────────▼──────────────────┐
│         Virtual Node Tree           │
│  RootNode → BlocksDirNode →         │
│  BlockNode → TxNode → InputNode ... │
└──────────────────┬──────────────────┘
                   │ cache miss
┌──────────────────▼──────────────────┐
│           Cache Layer               │
│  moka: volatile data + TTL          │
│  sled/redb: finalized, persistent   │
└──────────────────┬──────────────────┘
                   │ cache miss
┌──────────────────▼──────────────────┐
│           RPC Client                │
│  Bitcoin Core JSON-RPC + retry      │
│  (optional: Esplora HTTP adapter)   │
└──────────────────┬──────────────────┘
                   │
┌──────────────────▼──────────────────┐
│        Bitcoin Node / Indexer       │
│  Bitcoin Core / Esplora / QuickNode │
└─────────────────────────────────────┘
```

### Filesystem Structure

```
/
├── blocks/
│   ├── latest/               ← TTL 60s (one block time ~10 min, refresh faster)
│   │   ├── info.json
│   │   ├── hash
│   │   ├── height
│   │   ├── timestamp
│   │   └── txs/
│   ├── recent/               ← last 10 blocks (visible in ls)
│   │   ├── 840000/
│   │   ├── 839999/
│   │   └── ...
│   └── [height]/             ← ghost: not shown in ls, accessible by path
│       ├── info.json
│       ├── hash
│       ├── height
│       ├── timestamp
│       ├── merkle_root
│       └── txs/
│           └── [txid]/
│               ├── info.json
│               ├── inputs/
│               │   └── [n]/
│               │       ├── txid      ← previous output's txid
│               │       ├── vout      ← index of previous output
│               │       └── value     ← satoshis (resolved from prev tx)
│               └── outputs/
│                   └── [n]/
│                       ├── value     ← satoshis
│                       ├── address   ← decoded address (if standard script)
│                       └── script    ← hex scriptPubKey
├── txs/
│   └── [txid]/               ← ghost: not shown in ls
│       ├── info.json
│       ├── inputs/
│       │   └── [n]/
│       └── outputs/
│           └── [n]/
├── addresses/                ← requires address indexer
│   └── [address]/
│       ├── balance           ← satoshis (sum of UTXOs)
│       └── utxos.json        ← list of UTXOs
├── chain/
│   ├── height                ← current best block height
│   ├── hash                  ← current best block hash
│   ├── network               ← mainnet / testnet / signet
│   ├── difficulty
│   ├── fee/
│   │   ├── slow              ← sat/vByte, ~1 hour confirmation
│   │   ├── standard          ← sat/vByte, ~30 min
│   │   └── fast              ← sat/vByte, ~10 min (next block)
│   └── price/                ← optional; absent if no price API configured
│       ├── usd               ← BTC price in USD, decimal string
│       ├── eur               ← BTC price in EUR, decimal string
│       └── source            ← e.g. "coingecko"
└── mempool/                  ← [v2]
    ├── size                  ← number of transactions
    ├── bytes
    └── txs/
        └── [txid]/           ← ghost: not cached, always live
```

### Blockchain → Filesystem Mapping

| Blockchain Concept | Filesystem Representation |
|--------------------|--------------------------|
| Block | directory `blocks/[height]/` |
| Transaction | directory `txs/[txid]/` (also nested under block) |
| UTXO input | directory `txs/[txid]/inputs/[n]/` |
| UTXO output | directory `txs/[txid]/outputs/[n]/` |
| Address | directory `addresses/[address]/` |
| Chain metadata | directory `chain/` |
| Fee estimates | directory `chain/fee/` |
| Scalar field | plain text file (one value per file) |
| Group of fields | `info.json` file |

#### File Contents Per Node

**`blocks/[height]/`** — source: `getblockhash(height)` → `getblock(hash, 2)`

| File | Content | Type |
|------|---------|------|
| `info.json` | `{ height, hash, previousblockhash, timestamp, merkleroot, nonce, bits, difficulty, size, weight, ntx, version }` | JSON |
| `hash` | hex string, 64 chars | plain text |
| `height` | decimal integer | plain text |
| `timestamp` | decimal Unix timestamp | plain text |
| `merkle_root` | hex string | plain text |

**`txs/[txid]/`** — source: `getrawtransaction(txid, true)`

| File | Content | Type |
|------|---------|------|
| `info.json` | `{ txid, version, size, vsize, weight, locktime, fee, blockheight, confirmations }` | JSON |

`fee` is in satoshis (decimal integer). Requires resolving all inputs to sum input values.

**`txs/[txid]/inputs/[n]/`** — from `vin[n]` in the decoded transaction

| File | Content | Type |
|------|---------|------|
| `txid` | previous output txid | plain text |
| `vout` | previous output index, decimal | plain text |
| `value` | satoshis from previous output, decimal | plain text |
| `script` | hex scriptSig (or witness for SegWit) | plain text |

> Coinbase transactions: `inputs/0/` contains only `coinbase` (hex data, no `txid`/`vout`).

**`txs/[txid]/outputs/[n]/`** — from `vout[n]` in the decoded transaction

| File | Content | Type |
|------|---------|------|
| `value` | satoshis, decimal integer | plain text |
| `address` | decoded address (P2PKH/P2SH/bech32) — absent for undecodable scripts | plain text |
| `script` | hex scriptPubKey | plain text |

**`addresses/[address]/`** — source: Esplora `GET /address/{addr}/utxo` or Bitcoin Core with `-addressindex`

| File | Content | Type |
|------|---------|------|
| `balance` | satoshis, decimal integer | plain text |
| `utxos.json` | `[{ txid, vout, value, confirmations }]` | JSON |

**`chain/`** — source: `getblockchaininfo`, `getblockcount`, `getbestblockhash`

| File | Content | Type |
|------|---------|------|
| `height` | decimal block height | plain text |
| `hash` | hex best block hash | plain text |
| `network` | `mainnet` / `testnet` / `signet` | plain text |
| `difficulty` | decimal float string | plain text |

**`chain/fee/`** — source: `estimatesmartfee(n, "CONSERVATIVE")`

| File | Content | Type |
|------|---------|------|
| `slow` | sat/vByte, decimal, targets 6 blocks | plain text |
| `standard` | sat/vByte, decimal, targets 3 blocks | plain text |
| `fast` | sat/vByte, decimal, targets 1 block | plain text |

> `estimatesmartfee` returns BTC/kB; convert: `sat_per_vbyte = result * 1e8 / 1000`.

**`chain/price/`** — source: CoinGecko REST API (or configurable provider). Only present if `[price]` section exists in config. Entire directory returns `ENOENT` if not configured.

| File | Content | Type |
|------|---------|------|
| `usd` | BTC price in USD, decimal string (e.g. `65432.10`) | plain text |
| `eur` | BTC price in EUR, decimal string | plain text |
| `source` | provider name, e.g. `coingecko` | plain text |

> Prices are floating-point but stored as decimal strings with 2 decimal places — no precision loss relevant at this scale.

#### Format Principles

- **Scalar files**: plain text terminated with `\n` — pipeable to Unix tools.
- **JSON files**: pretty-printed with 2-space indent — readable without `jq`.
- **All amounts**: satoshis as decimal integer — never BTC float (avoids precision loss). Convert to BTC: `echo "$(cat balance) / 100000000" | bc -l`.
- **Hashes**: lowercase hex — consistent with Bitcoin Core output.

### Example Outputs

```
$ cat chain/height
840000

$ cat chain/network
mainnet

$ cat chain/fee/fast
45

$ cat blocks/840000/info.json
{
  "height": 840000,
  "hash": "0000000000000000000320283a032748cef8227873ff4872689bf23f1cda83a5",
  "previousblockhash": "00000000000000000002a23d6df20eecec15b21d32c75833cce28f113de888b7",
  "timestamp": 1713571767,
  "merkleroot": "050bb3e51f8f7a0c94e3012c95e03f6f4a0e0e0a5a5b5c5d5e5f5a5b5c5d5e5f",
  "nonce": 3932395645,
  "bits": "17034219",
  "difficulty": "83148355189239.02",
  "size": 1563039,
  "weight": 3991993,
  "ntx": 3721,
  "version": 798720000
}

$ ls blocks/840000/txs/
a1b2c3d4...
e5f6a7b8...
...

$ cat txs/a1b2c3d4.../info.json
{
  "txid": "a1b2c3d4...",
  "version": 1,
  "size": 225,
  "vsize": 141,
  "weight": 561,
  "locktime": 0,
  "fee": 4230,
  "blockheight": 840000,
  "confirmations": 1042
}

$ ls txs/a1b2c3d4.../inputs/
0

$ cat txs/a1b2c3d4.../inputs/0/value
100000000

$ ls txs/a1b2c3d4.../outputs/
0
1

$ cat txs/a1b2c3d4.../outputs/0/value
99995770

$ cat txs/a1b2c3d4.../outputs/0/address
bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh

$ cat addresses/bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh/balance
250000000

$ cat addresses/bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh/utxos.json
[
  {
    "txid": "a1b2c3d4...",
    "vout": 0,
    "value": 250000000,
    "confirmations": 1042
  }
]
```

---

## 3. Implementation

### Node Design

The filesystem is structured as a virtual node tree. In Rust, `fuser` uses a single `Filesystem` trait — all FUSE operations (`lookup`, `readdir`, `getattr`, `read`) dispatch centrally to per-node handlers based on inode.

> **Concurrency constraint:** `fuser` dispatches FUSE requests from multiple threads concurrently. The `BtcFs` struct that implements `fuser::Filesystem` must be `Send + Sync + 'static`. All shared state (`BtcClient`, `Cache`) must be wrapped in `Arc<T>` where `T: Send + Sync`.

```
fuser::Filesystem trait methods:
  lookup(parent_ino, name) → dispatch to parent node handler
  readdir(ino)             → list children of node
  getattr(ino)             → file attributes (mode, size, mtime)
  read(ino, offset, size)  → file contents
```

```
RootNode
  └── BlocksDirNode       (lookup + readdir)
        ├── RecentDirNode (readdir → last 10 blocks)
        ├── LatestDirNode (readdir → TTL 60s)
        └── BlockNode     (lookup + readdir + getattr)
              ├── FileNode: info.json, hash, height, timestamp, merkle_root
              └── TxsDirNode
                    └── TxNode      (lookup + readdir)
                          ├── FileNode: info.json
                          ├── InputsDirNode
                          │     └── InputNode (readdir)
                          │           └── FileNode: txid, vout, value, script
                          └── OutputsDirNode
                                └── OutputNode (readdir)
                                      └── FileNode: value, address, script
  └── TxsDirNode          (lookup only — ghost, no readdir)
        └── TxNode        (same as above)
  └── AddressesDirNode    (lookup only — ghost)
        └── AddressNode   (readdir)
              └── FileNode: balance, utxos.json
  └── ChainNode           (readdir)
        ├── FileNode: height, hash, network, difficulty
        └── FeeDirNode    (readdir)
              └── FileNode: slow, standard, fast  (read, TTL 30s)
  └── MempoolNode         (readdir) [v2]
        ├── FileNode: size, bytes
        └── TxsDirNode
              └── TxNode  (ghost, uncached)
```

### Request Lifecycle

Flow for `cat /txs/{txid}/outputs/0/value`:

```
kernel (VFS)
    │  read("/txs/{txid}/outputs/0/value")
    ▼
FUSE Layer (fuser)
    │  dispatch to Filesystem::read(ino)
    ▼
TxsDirNode.Lookup(txid)
    │
    ├─── negative cache hit? ─────────────────────────► ENOENT (fast path)
    │
    ├─── sled hit? ───────────────────────────────────► return cached tx
    │
    │    sled miss
    ├─── moka hit? ───────────────────────────────────► return cached tx
    │
    │    full miss
    ▼
RPCClient.GetRawTransaction(txid)
    │
    ├─── error? ──── not found  → set negative cache → ENOENT
    │                timeout    → EIO
    │                rate limit → EAGAIN
    │
    ▼  success
decode transaction
    │
    ├─── confirmations >= 6 → sled (permanent)
    └─── confirmations < 6  → moka (TTL 60s)
    │
    ▼
OutputNode::read() → outputs[0].value → b"100000000\n"
return to kernel
```

> **Negative cache must be checked first**, before both moka and sled, on every lookup path. This prevents redundant persistent storage reads for known-missing keys.

### Address Resolution Flow

`cat /addresses/bc1q.../balance` requires an extra step because Bitcoin balance = UTXO sum:

```
AddressNode.Read("balance")
    │
    ├─── ttlcache hit? ──────────────────────────────► return cached balance
    │
    │    miss
    ▼
IndexerClient.UTXOs(address)   ← Esplora or Bitcoin Core addressindex
    │
    ▼
sum(utxo.value for utxo in utxos)
    │
    ├─── write to ttlcache (TTL 30s)
    ▼
return satoshis as decimal string
```

> If no indexer is configured: `addresses/` directory exists but all lookups return `ENOENT` with a clear error log.

### Cache TTL Strategy

| Data | TTL | Storage |
|------|-----|---------|
| `chain/height` | 30s | moka |
| `blocks/latest/` | 60s | moka |
| `chain/fee/` | 30s | moka |
| Address balance / UTXOs | 30s | moka |
| `chain/price/` | 60s | moka |
| Negative cache | 30s | moka |
| Unconfirmed tx | 60s | moka |
| Block data (finalized, ≥6 conf) | ∞ | sled |
| Tx data (finalized, ≥6 conf) | ∞ | sled |
| Mempool tx | not cached | — |

> Bitcoin block time is ~10 minutes; finality at 6 blocks takes ~1 hour. A 60s TTL for `blocks/latest/` gives reasonable freshness without hammering the node.

### Cache Serialization

All values stored in moka and sled are serialized to bytes using `bincode` (compact binary, no schema). `Block` and `Transaction` must derive `serde::Serialize + serde::Deserialize`. Never store raw `f64` — convert fee rates to `u64` satoshis before serializing.

### Two-Tier Cache Fetch Pattern

`fetch_block` and `fetch_tx` both share the same lookup flow. Extract a generic helper to avoid copy-paste:

```rust
fn fetch_cached<T, F>(
    cache: &Cache,
    mem_key: &str,
    persist_bucket: &str,
    latest_height: u64,
    item_height: u64,
    rpc_fn: F,
) -> Result<T, BtcFuseError>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce() -> Result<T, BtcFuseError>,
{
    // 1. negative cache check
    // 2. moka check
    // 3. sled check (populate moka on hit)
    // 4. call rpc_fn
    // 5. store: sled if finalized (latest_height - item_height >= 6), moka otherwise
}
```

### `LatestDirNode` Delegation Pattern

`LatestDirNode` must not duplicate `BlockNode`'s `lookup`/`readdir` logic. Instead, resolve the current tip height and delegate entirely to `BlockNode`:

```rust
impl LatestDirNode {
    fn lookup(&self, name: &str) -> Result<Node> {
        let height = self.cache.get_or_fetch_height()?;
        let block_node = BlockNode::new(height, ...);
        block_node.lookup(name)  // delegate, no duplication
    }
}
```

### No RPC Calls in Constructors

Node constructors (`BlockNode::new`, `ChainNode::new`, etc.) must not make RPC calls. Chain metadata (`network`) is fetched once at startup and stored in the cache before mounting. Constructors only receive `Arc<dyn BtcClient>` and `Arc<Cache>`.

### Error Types

Use `thiserror` for typed errors; map to FUSE errno in one place:

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
            BtcFuseError::NotFound       => libc::ENOENT,
            BtcFuseError::InvalidFormat(_) => libc::EINVAL,
            BtcFuseError::RateLimit      => libc::EAGAIN,
            BtcFuseError::Rpc(_)         => libc::EIO,
            BtcFuseError::Cache(_)       => libc::EIO,
        }
    }
}
```

Never use `strings.Contains`-style pattern matching on error messages to determine errno.

### Inode Numbering

Deterministic hash from path using FNV-1a 64-bit. Use the `fnv` crate (`fnv::FnvHasher`). Root inode = 1 (required by FUSE).

### Error Mapping

| Condition | FUSE errno |
|-----------|------------|
| Block/tx/address not found | `ENOENT` |
| RPC rate limit / 429 | `EAGAIN` |
| Timeout / network error | `EIO` |
| Invalid address or txid format | `EINVAL` |
| Write to read-only filesystem | `EROFS` |
| No indexer configured for `addresses/` | `ENOENT` + log warning |

All errno mapping is done in a single `From<BtcFuseError> for libc::c_int` impl (see Implementation Patterns). Never match on error message strings.

---

## 4. Testing Strategy

### Layers and Boundaries

```
┌──────────────────────────────────────────────────────┐
│                   Integration tests                  │
│   mount real FUSE + real Bitcoin node (testnet)      │
│   verify actual file contents from terminal          │
├──────────────────────────────────────────────────────┤
│                     Unit tests                       │
│   each node type in isolation with MockRPCClient     │
│   no FUSE, no network, no disk                       │
├──────────────────────────────────────────────────────┤
│                     RPC tests                        │
│   Client retry behavior with fake HTTP server        │
│   error sanitization (RPC credentials not leaked)    │
└──────────────────────────────────────────────────────┘
```

### Unit Tests

| Node | What to test |
|------|-------------|
| `BlockNode` | `info.json` fields match RPC response; `height`, `hash`, `timestamp` terminated with `\n`; `ENOENT` for unknown height |
| `TxNode` | `info.json` built from block cache; inputs/outputs index correctly; coinbase tx has no `txid` in `inputs/0/` |
| `InputNode` | `value` resolved from previous output; satoshis as decimal integer |
| `OutputNode` | `value` decimal; `address` absent for undecodable script; `script` as hex |
| `AddressNode` | `balance` = sum of UTXO values; `EINVAL` for malformed address; TTL 30s |
| `ChainNode` | `height` reflects background refresh; fee files use correct `estimatesmartfee` targets |
| Cache layer | moka TTL expires; sled survives restart; negative cache blocks repeated RPC calls |
| `fetch_cached` | negative cache prevents moka + sled reads; sled hit populates moka; finalized tx goes to sled |

### Integration Tests

```bash
BTCFS_RPC_URL=http://user:pass@localhost:18332 cargo test --test integration
```

RPC endpoint options (set via `BTCFS_RPC_URL`):
- **Bitcoin Core** on testnet4 or signet — self-hosted, no rate limits, primary target
- **QuickNode** — managed Bitcoin node, compatible with Bitcoin Core JSON-RPC, equivalent to Alchemy for Ethereum

Tests skip if `BTCFS_RPC_URL` is not set.

| Scenario | What to verify |
|----------|---------------|
| `cat chain/height` | decimal integer, increments over time |
| `cat blocks/{height}/info.json` | valid JSON, matches known block |
| `ls blocks/{height}/txs/` | correct tx count |
| `cat txs/{txid}/outputs/0/value` | satoshis match known tx |
| `cat txs/{coinbase_txid}/inputs/0/coinbase` | file exists; `txid` file absent |
| Write attempt | returns `EROFS` |
| Invalid txid | returns `EINVAL` |
| Ghost block accessible by height | accessible by path, not in `ls` |

---

## 5. Dependencies

### Library Decisions

| Component | Library | Reason |
|-----------|---------|--------|
| FUSE | `fuser` | Actively maintained Rust FUSE3 crate; trait-based API |
| Bitcoin primitives | `rust-bitcoin` | Canonical Rust Bitcoin library: tx encoding, script, address (P2PKH/bech32/P2TR) |
| Bitcoin RPC client | `bitcoincore-rpc` | Official rust-bitcoin ecosystem client for Bitcoin Core JSON-RPC |
| Cache (memory) | `moka` | Per-key TTL, concurrent, async-friendly |
| Cache (disk) | `sled` or `redb` | Embedded key-value store, pure Rust, ACID |
| Cache serialization | `bincode` | Compact binary serialization for Block/Transaction stored in sled |
| Rate limiting | `governor` | Token-bucket rate limiter; applied inside concrete `BtcClient` impls |
| CLI | `clap` | De facto standard Rust CLI library |
| Config | `toml` crate + `std::env` | Lightweight TOML parsing |
| Logging | `tracing` | Standard Rust structured logging |
| Error handling | `thiserror` | Typed error enum with single errno mapping; no anyhow at FUSE boundaries |
| Price API (optional) | `reqwest` | HTTP client for CoinGecko REST API |

### RPC Backend Options

| Backend | Address Indexing | Notes |
|---------|-----------------|-------|
| Bitcoin Core JSON-RPC | Only with `-addressindex` | Primary target; self-hosted |
| QuickNode | Yes | Managed Bitcoin node; JSON-RPC compatible; use for integration tests without self-hosted node |
| Esplora (Blockstream) | Yes — REST API | Good for address/UTXO queries; rate-limited public endpoint |
| Mempool.space API | Yes | Good fee estimates and mempool data |

Config snippet for price API (`~/.config/btcfuse/config.toml`):

```toml
[price]
provider = "coingecko"   # only supported provider in v1
currencies = ["usd", "eur"]
```

If `[price]` section is absent, `chain/price/` does not exist in the filesystem (`ENOENT`).

Define a `BtcClient` trait; implement at least one concrete adapter. The FUSE layer depends only on the trait, never on a concrete type.

```rust
pub trait BtcClient: Send + Sync {
    fn get_block_count(&self) -> Result<u64>;
    fn get_best_block_hash(&self) -> Result<BlockHash>;
    fn get_block_hash(&self, height: u64) -> Result<BlockHash>;
    fn get_block(&self, hash: &BlockHash) -> Result<Block>;
    fn get_raw_transaction(&self, txid: &Txid) -> Result<Transaction>;
    fn get_blockchain_info(&self) -> Result<BlockchainInfo>;
    fn estimate_smart_fee(&self, blocks: u16) -> Result<u64>;  // sat/vByte, already converted
    // Optional — only implemented by indexer-backed adapters:
    fn get_address_utxos(&self, addr: &Address) -> Result<Vec<Utxo>>;
}
```
