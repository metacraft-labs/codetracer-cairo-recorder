//! The block a transaction sits in, as the executor needs it: header -> `BlockContext`,
//! bodies -> executable transactions, and the chain's own answers to compare against.

use blockifier::blockifier_versioned_constants::VersionedConstants;
use blockifier::bouncer::BouncerConfig;
use blockifier::context::{BlockContext, ChainInfo, FeeTokenAddresses};
use blockifier::transaction::account_transaction::ExecutionFlags;
use blockifier::transaction::transaction_execution::Transaction as BlockifierTransaction;
use eyre::{bail, eyre, Result};
use serde_json::{json, Value};
use starknet_api::block::{
    BlockHash, BlockHashAndNumber, BlockInfo, BlockNumber, BlockTimestamp, GasPrice,
    GasPriceVector, GasPrices, NonzeroGasPrice, StarknetVersion,
};
use starknet_api::contract_class::ClassInfo;
use starknet_api::core::{ChainId, ContractAddress};
use starknet_api::fee_token_defaults::{ETH_FEE_CONTRACT_ADDRESS, STRK_FEE_CONTRACT_ADDRESS};
use starknet_api::serde_utils::deserialize_transaction_json_to_starknet_api_tx;
use starknet_api::transaction::fields::Fee;
use starknet_api::transaction::{Transaction, TransactionHash};
use starknet_api::versioned_constants_logic::VersionedConstantsTrait;
use starknet_types_core::felt::Felt;

use crate::rpc::Rpc;
use crate::state::RpcState;

/// Blocks the OS keeps a hash for, behind the current one (`STORED_BLOCK_HASH_BUFFER`).
const STORED_BLOCK_HASH_BUFFER: u64 = 10;

/// The fee an L1 handler is credited with on L1 — the executor requires one and the
/// receipt does not carry it; StarkWare's re-execution uses the same ceiling.
const MAX_FEE_FOR_L1_HANDLER: Fee = Fee(100_000_000_000_000_000);

pub struct Block {
    pub number: u64,
    pub header: Value,
    pub info: BlockInfo,
    pub starknet_version: String,
    /// `(hash, raw RPC body, receipt)` in block order.
    pub txs: Vec<(String, Value, Value)>,
    /// Header gas prices that were zero and were substituted with the minimum.
    pub substituted_gas_prices: Vec<String>,
}

/// One header gas price. A ZERO price — which headers carry for dimensions the era did
/// not charge (measured: block 480,000, Starknet 0.12.3) — is not representable in the
/// executor's `NonzeroGasPrice`, so it is substituted with the minimum and the
/// substitution is RECORDED by name in `substituted`, never silently. If the
/// substitution ever mattered, the fee comparison — a counted fact — would show it.
fn gas(v: &Value, name: &str, substituted: &mut Vec<String>) -> Result<NonzeroGasPrice> {
    let s = v
        .as_str()
        .ok_or_else(|| eyre!("gas price {name} = {v} is not a string"))?;
    let n = u128::from_str_radix(s.trim_start_matches("0x"), 16)?;
    if n == 0 {
        substituted.push(name.to_string());
        return Ok(NonzeroGasPrice::MIN);
    }
    NonzeroGasPrice::new(GasPrice(n)).map_err(|e| eyre!("gas price {name} = {s}: {e}"))
}

pub fn fetch_block(rpc: &Rpc, number: u64) -> Result<Block> {
    let header = rpc.call(
        "starknet_getBlockWithReceipts",
        json!([{ "block_number": number }]),
    )?;
    let sv = header["starknet_version"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let starknet_version: StarknetVersion = sv
        .clone()
        .try_into()
        .map_err(|e| eyre!("block {number}: starknet_version {sv:?}: {e:?}"))?;
    let mut sub = vec![];
    let info = BlockInfo {
        block_number: BlockNumber(number),
        block_timestamp: BlockTimestamp(header["timestamp"].as_u64().unwrap_or(0)),
        sequencer_address: ContractAddress::try_from(Felt::from_hex(
            header["sequencer_address"].as_str().unwrap_or("0x0"),
        )?)?,
        gas_prices: GasPrices {
            eth_gas_prices: GasPriceVector {
                l1_gas_price: gas(
                    &header["l1_gas_price"]["price_in_wei"],
                    "l1_gas_price.price_in_wei",
                    &mut sub,
                )?,
                l1_data_gas_price: gas(
                    &header["l1_data_gas_price"]["price_in_wei"],
                    "l1_data_gas_price.price_in_wei",
                    &mut sub,
                )?,
                l2_gas_price: gas(
                    &header["l2_gas_price"]["price_in_wei"],
                    "l2_gas_price.price_in_wei",
                    &mut sub,
                )?,
            },
            strk_gas_prices: GasPriceVector {
                l1_gas_price: gas(
                    &header["l1_gas_price"]["price_in_fri"],
                    "l1_gas_price.price_in_fri",
                    &mut sub,
                )?,
                l1_data_gas_price: gas(
                    &header["l1_data_gas_price"]["price_in_fri"],
                    "l1_data_gas_price.price_in_fri",
                    &mut sub,
                )?,
                l2_gas_price: gas(
                    &header["l2_gas_price"]["price_in_fri"],
                    "l2_gas_price.price_in_fri",
                    &mut sub,
                )?,
            },
        },
        use_kzg_da: header["l1_da_mode"].as_str() == Some("BLOB"),
        starknet_version,
    };
    let mut txs = vec![];
    for t in header["transactions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        let body = t["transaction"].clone();
        let receipt = t["receipt"].clone();
        let hash = receipt["transaction_hash"]
            .as_str()
            .or_else(|| body["transaction_hash"].as_str())
            .ok_or_else(|| eyre!("block {number}: a transaction with no hash"))?
            .to_string();
        txs.push((hash, body, receipt));
    }
    Ok(Block {
        number,
        header,
        info,
        starknet_version: sv,
        txs,
        substituted_gas_prices: sub,
    })
}

pub fn chain_info() -> ChainInfo {
    ChainInfo {
        chain_id: ChainId::Mainnet,
        fee_token_addresses: FeeTokenAddresses {
            strk_fee_token_address: *STRK_FEE_CONTRACT_ADDRESS,
            eth_fee_token_address: *ETH_FEE_CONTRACT_ADDRESS,
        },
        is_l3: false,
    }
}

pub fn block_context(block: &Block) -> Result<BlockContext> {
    let mut vc = VersionedConstants::get(&block.info.starknet_version)
        .map_err(|e| {
            eyre!(
                "block {}: the executor carries no versioned constants for Starknet {}: {e:?}",
                block.number,
                block.starknet_version
            )
        })?
        .clone();
    // As StarkWare's re-execution does: a recompiled class is not migrated mid-replay.
    vc.disable_casm_hash_migration();
    Ok(BlockContext::new(
        block.info.clone(),
        chain_info(),
        vc,
        BouncerConfig::max(),
    ))
}

pub fn old_block_hash(rpc: &Rpc, number: u64) -> Result<Option<BlockHashAndNumber>> {
    if number < STORED_BLOCK_HASH_BUFFER {
        return Ok(None);
    }
    let old = number - STORED_BLOCK_HASH_BUFFER;
    let h = rpc.call(
        "starknet_getBlockWithTxHashes",
        json!([{ "block_number": old }]),
    )?;
    let hash = Felt::from_hex(h["block_hash"].as_str().unwrap_or("0x0"))?;
    Ok(Some(BlockHashAndNumber {
        number: BlockNumber(old),
        hash: BlockHash(hash),
    }))
}

/// One RPC body -> one executable transaction.
pub fn executable(state: &RpcState, hash: &str, body: &Value) -> Result<BlockifierTransaction> {
    let tx_hash = TransactionHash(Felt::from_hex(hash)?);
    let api: Transaction = deserialize_transaction_json_to_starknet_api_tx(body.clone())
        .map_err(|e| eyre!("{hash}: the body does not deserialize: {e}"))?;
    let flags = ExecutionFlags::default();
    let tx = match api {
        Transaction::Invoke(_) | Transaction::DeployAccount(_) => {
            BlockifierTransaction::from_api(api, tx_hash, None, None, None, flags)
        }
        Transaction::Declare(ref d) => {
            let rec = state
                .class_record(d.class_hash())
                .map_err(|e| eyre!("{hash}: the declared class: {e}"))?;
            let (sierra_len, abi_len) = match &rec.casm {
                Some(_) => (
                    rec.raw["sierra_program"]
                        .as_array()
                        .map(|a| a.len())
                        .unwrap_or(0),
                    rec.raw["abi"].as_str().map(|s| s.len()).unwrap_or(0),
                ),
                None => (
                    starknet_api::contract_class::DEPRECATED_CONTRACT_SIERRA_SIZE,
                    rec.raw["abi"].to_string().len(),
                ),
            };
            let sv = rec
                .sierra_version
                .clone()
                .unwrap_or(starknet_api::contract_class::SierraVersion::DEPRECATED);
            let ci = ClassInfo::new(&rec.contract_class(), sierra_len, abi_len, sv)
                .map_err(|e| eyre!("{hash}: class info: {e}"))?;
            BlockifierTransaction::from_api(api, tx_hash, Some(ci), None, None, flags)
        }
        Transaction::L1Handler(_) => BlockifierTransaction::from_api(
            api,
            tx_hash,
            None,
            Some(MAX_FEE_FOR_L1_HANDLER),
            None,
            flags,
        ),
        Transaction::Deploy(_) => {
            bail!("{hash}: a DEPLOY transaction (pre-0.10) is not executable")
        }
    };
    tx.map_err(|e| eyre!("{hash}: {e}"))
}
