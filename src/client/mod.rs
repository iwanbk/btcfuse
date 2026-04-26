#![allow(dead_code)]

pub mod core;

use bitcoin::{BlockHash, Txid};
use serde::{Deserialize, Serialize};

use crate::error::BtcFuseError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockchainInfo {
    pub chain: String,
    pub blocks: u64,
    pub best_block_hash: BlockHash,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub hash: BlockHash,
    pub height: u64,
    pub time: u64,
    pub tx: Vec<Txid>,
    pub confirmations: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub txid: Option<Txid>,
    pub vout: Option<u32>,
    /// satoshis; None for coinbase
    pub value: Option<u64>,
    pub coinbase: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Output {
    /// satoshis
    pub value: u64,
    pub script_pubkey: String,
    pub address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub txid: Txid,
    pub block_hash: Option<BlockHash>,
    pub confirmations: i64,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Utxo {
    pub txid: Txid,
    pub vout: u32,
    /// satoshis
    pub value: u64,
}

pub trait BtcClient: Send + Sync {
    fn get_blockchain_info(&self) -> Result<BlockchainInfo, BtcFuseError>;
    fn get_block_count(&self) -> Result<u64, BtcFuseError>;
    fn get_block_hash(&self, height: u64) -> Result<BlockHash, BtcFuseError>;
    fn get_block(&self, hash: &BlockHash) -> Result<Block, BtcFuseError>;
    fn get_transaction(&self, txid: &Txid) -> Result<Transaction, BtcFuseError>;
}
