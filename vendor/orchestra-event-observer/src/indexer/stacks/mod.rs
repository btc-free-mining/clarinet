mod blocks_pool;

pub use blocks_pool::StacksBlockPool;

use crate::indexer::AssetClassCache;
use crate::indexer::{IndexerConfig, StacksChainContext};
use bitcoincore_rpc::bitcoin::Block;
use clarinet_utils::transactions::{StacksTransaction, TransactionAuth, TransactionPayload};
use clarity_repl::clarity::codec::StacksMessageCodec;
use clarity_repl::clarity::util::hash::hex_bytes;
use clarity_repl::clarity::vm::Value as ClarityValue;
use orchestra_types::*;
use rocket::serde::json::Value as JsonValue;
use rocket::serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::convert::TryInto;
use std::io::Cursor;
use std::str;

#[derive(Deserialize)]
pub struct NewBlock {
    pub block_height: u64,
    pub block_hash: String,
    pub index_block_hash: String,
    pub burn_block_height: u64,
    pub burn_block_hash: String,
    pub parent_block_hash: String,
    pub parent_index_block_hash: String,
    pub parent_microblock: String,
    pub parent_microblock_sequence: u64,
    pub parent_burn_block_hash: String,
    pub parent_burn_block_height: u64,
    pub parent_burn_block_timestamp: u64,
    pub transactions: Vec<NewTransaction>,
    pub events: Vec<NewEvent>,
    pub matured_miner_rewards: Vec<MaturedMinerReward>,
}

#[derive(Deserialize)]
pub struct MaturedMinerReward {
    pub from_index_consensus_hash: String,
    pub from_stacks_block_hash: String,
    pub recipient: String,
    pub coinbase_amount: String,
    /// micro-STX amount
    pub tx_fees_anchored: String,
    /// micro-STX amount
    pub tx_fees_streamed_confirmed: String,
    /// micro-STX amount
    pub tx_fees_streamed_produced: String,
}

#[derive(Deserialize, Debug)]
pub struct NewMicroblockTrail {
    pub parent_index_block_hash: String,
    pub burn_block_hash: String,
    pub burn_block_height: u64,
    pub burn_block_timestamp: u64,
    pub transactions: Vec<NewMicroblockTransaction>,
    pub events: Vec<NewEvent>,
}

#[derive(Deserialize)]
pub struct NewTransaction {
    pub txid: String,
    pub tx_index: usize,
    pub status: String,
    pub raw_result: String,
    pub raw_tx: String,
    pub execution_cost: Option<StacksTransactionExecutionCost>,
}

#[derive(Deserialize, Debug)]
pub struct NewMicroblockTransaction {
    pub txid: String,
    pub tx_index: usize,
    pub status: String,
    pub raw_result: String,
    pub raw_tx: String,
    pub execution_cost: Option<StacksTransactionExecutionCost>,
    pub microblock_sequence: usize,
    pub microblock_hash: String,
    pub microblock_parent_hash: String,
}

#[derive(Debug, Deserialize)]
pub struct NewEvent {
    pub txid: String,
    pub committed: bool,
    pub event_index: u32,
    #[serde(rename = "type")]
    pub event_type: String,
    pub stx_transfer_event: Option<JsonValue>,
    pub stx_mint_event: Option<JsonValue>,
    pub stx_burn_event: Option<JsonValue>,
    pub stx_lock_event: Option<JsonValue>,
    pub nft_transfer_event: Option<JsonValue>,
    pub nft_mint_event: Option<JsonValue>,
    pub nft_burn_event: Option<JsonValue>,
    pub ft_transfer_event: Option<JsonValue>,
    pub ft_mint_event: Option<JsonValue>,
    pub ft_burn_event: Option<JsonValue>,
    pub data_var_set_event: Option<JsonValue>,
    pub data_map_insert_event: Option<JsonValue>,
    pub data_map_update_event: Option<JsonValue>,
    pub data_map_delete_event: Option<JsonValue>,
    pub contract_event: Option<JsonValue>,
}

pub fn get_stacks_currency() -> Currency {
    Currency {
        symbol: "STX".into(),
        decimals: 6,
        metadata: None,
    }
}

#[derive(Deserialize, Debug)]
pub struct ContractReadonlyCall {
    pub okay: bool,
    pub result: String,
}

pub fn standardize_stacks_block(
    indexer_config: &IndexerConfig,
    marshalled_block: JsonValue,
    ctx: &mut StacksChainContext,
) -> StacksBlockData {
    let mut block: NewBlock = serde_json::from_value(marshalled_block).unwrap();

    let pox_cycle_length: u64 =
        (ctx.pox_info.prepare_phase_block_length + ctx.pox_info.reward_phase_block_length).into();
    let current_len = block.burn_block_height - ctx.pox_info.first_burnchain_block_height;
    let pox_cycle_id: u32 = (current_len / pox_cycle_length).try_into().unwrap();

    let mut events = vec![];
    events.append(&mut block.events);
    let transactions = block
        .transactions
        .iter()
        .map(|tx| {
            let (description, tx_type, fee, sender, sponsor) =
                get_tx_description(&tx.raw_tx).expect("unable to parse transaction");
            let (operations, receipt) = get_standardized_stacks_operations(
                &tx.txid,
                &mut events,
                &mut ctx.asset_class_map,
                &indexer_config.stacks_node_rpc_url,
            );
            StacksTransactionData {
                transaction_identifier: TransactionIdentifier {
                    hash: tx.txid.clone(),
                },
                operations,
                metadata: StacksTransactionMetadata {
                    success: tx.status == "success",
                    result: get_value_description(&tx.raw_result),
                    raw_tx: tx.raw_tx.clone(),
                    sender,
                    fee,
                    sponsor,
                    kind: tx_type,
                    execution_cost: tx.execution_cost.clone(),
                    receipt,
                    description,
                    position: StacksTransactionPosition::Index(tx.tx_index),
                },
            }
        })
        .collect();

    let confirm_microblock_identifier = if block.parent_microblock
        == "0x0000000000000000000000000000000000000000000000000000000000000000"
    {
        None
    } else {
        Some(BlockIdentifier {
            index: block
                .parent_microblock_sequence
                .try_into()
                .expect("unable to get microblock sequence"),
            hash: block.parent_microblock.clone(),
        })
    };

    StacksBlockData {
        block_identifier: BlockIdentifier {
            hash: block.index_block_hash.clone(),
            index: block.block_height,
        },
        parent_block_identifier: BlockIdentifier {
            hash: block.parent_index_block_hash.clone(),
            index: block.block_height - 1,
        },
        timestamp: 0,
        metadata: StacksBlockMetadata {
            bitcoin_anchor_block_identifier: BlockIdentifier {
                hash: block.burn_block_hash.clone(),
                index: block.burn_block_height,
            },
            pox_cycle_index: pox_cycle_id,
            pox_cycle_position: (current_len % pox_cycle_length) as u32,
            pox_cycle_length: pox_cycle_length.try_into().unwrap(),
            confirm_microblock_identifier,
        },
        transactions,
    }
}

pub fn standardize_stacks_microblock_trail(
    indexer_config: &IndexerConfig,
    marshalled_microblock_trail: JsonValue,
    ctx: &mut StacksChainContext,
) -> Vec<StacksMicroblockData> {
    let mut microblock_trail: NewMicroblockTrail =
        serde_json::from_value(marshalled_microblock_trail).unwrap();

    let mut events = vec![];
    events.append(&mut microblock_trail.events);

    let mut microblocks_set: BTreeMap<
        (BlockIdentifier, BlockIdentifier),
        Vec<StacksTransactionData>,
    > = BTreeMap::new();
    for tx in microblock_trail.transactions.iter() {
        let (description, tx_type, fee, sender, sponsor) =
            get_tx_description(&tx.raw_tx).expect("unable to parse transaction");
        let (operations, receipt) = get_standardized_stacks_operations(
            &tx.txid,
            &mut events,
            &mut ctx.asset_class_map,
            &indexer_config.stacks_node_rpc_url,
        );

        let microblock_identifier = BlockIdentifier {
            hash: tx.microblock_hash.clone(),
            index: u64::try_from(tx.microblock_sequence).unwrap(),
        };

        let parent_microblock_identifier = if tx.microblock_sequence > 0 {
            BlockIdentifier {
                hash: tx.microblock_parent_hash.clone(),
                index: microblock_identifier.index.saturating_sub(1),
            }
        } else {
            microblock_identifier.clone()
        };

        let transaction = StacksTransactionData {
            transaction_identifier: TransactionIdentifier {
                hash: tx.txid.clone(),
            },
            operations,
            metadata: StacksTransactionMetadata {
                success: tx.status == "success",
                result: get_value_description(&tx.raw_result),
                raw_tx: tx.raw_tx.clone(),
                sender,
                fee,
                sponsor,
                kind: tx_type,
                execution_cost: tx.execution_cost.clone(),
                receipt,
                description,
                position: StacksTransactionPosition::Microblock(
                    microblock_identifier.clone(),
                    tx.tx_index,
                ),
            },
        };

        microblocks_set
            .entry((microblock_identifier, parent_microblock_identifier))
            .and_modify(|transactions| transactions.push(transaction.clone()))
            .or_insert(vec![transaction]);
    }

    let mut microblocks = vec![];
    for ((block_identifier, parent_block_identifier), transactions) in microblocks_set.into_iter() {
        microblocks.push(StacksMicroblockData {
            block_identifier,
            parent_block_identifier,
            timestamp: 0,
            transactions,
            metadata: StacksMicroblockMetadata {
                anchor_block_identifier: BlockIdentifier {
                    hash: microblock_trail.parent_index_block_hash.clone(),
                    index: 0,
                },
            },
        })
    }
    microblocks.sort_by(|a, b| a.block_identifier.cmp(&b.block_identifier));

    microblocks
}

pub fn get_value_description(raw_value: &str) -> String {
    let raw_value = match raw_value.strip_prefix("0x") {
        Some(raw_value) => raw_value,
        _ => return raw_value.to_string(),
    };
    let value_bytes = match hex_bytes(&raw_value) {
        Ok(bytes) => bytes,
        _ => return raw_value.to_string(),
    };

    let value = match ClarityValue::consensus_deserialize(&mut Cursor::new(&value_bytes)) {
        Ok(value) => format!("{}", value),
        Err(e) => {
            error!("unable to deserialize clarity value {:?}", e);
            return raw_value.to_string();
        }
    };
    value
}

pub fn get_tx_description(
    raw_tx: &str,
) -> Result<
    (
        String, // Human readable transaction's description (contract-call, publish, ...)
        StacksTransactionKind, //
        u64,    // Transaction fee
        String, // Sender's address
        Option<String>, // Sponsor's address (optional)
    ),
    (),
> {
    let raw_tx = match raw_tx.strip_prefix("0x") {
        Some(raw_tx) => raw_tx,
        _ => return Err(()),
    };
    let tx_bytes = match hex_bytes(&raw_tx) {
        Ok(bytes) => bytes,
        _ => return Err(()),
    };
    let tx = match StacksTransaction::consensus_deserialize(&mut Cursor::new(&tx_bytes)) {
        Ok(bytes) => bytes,
        _ => return Err(()),
    };

    let (fee, sender, sponsor) = match tx.auth {
        TransactionAuth::Standard(ref conditions) => (
            conditions.tx_fee(),
            if tx.is_mainnet() {
                conditions.address_mainnet().to_string()
            } else {
                conditions.address_testnet().to_string()
            },
            None,
        ),
        TransactionAuth::Sponsored(ref sender_conditions, ref sponsor_conditions) => (
            sponsor_conditions.tx_fee(),
            if tx.is_mainnet() {
                sender_conditions.address_mainnet().to_string()
            } else {
                sender_conditions.address_testnet().to_string()
            },
            Some(if tx.is_mainnet() {
                sponsor_conditions.address_mainnet().to_string()
            } else {
                sponsor_conditions.address_testnet().to_string()
            }),
        ),
    };

    let (description, tx_type) = match tx.payload {
        TransactionPayload::TokenTransfer(ref addr, ref amount, ref _memo) => (
            format!(
                "transfered: {} µSTX from {} to {}",
                amount,
                tx.origin_address(),
                addr
            ),
            StacksTransactionKind::NativeTokenTransfer,
        ),
        TransactionPayload::ContractCall(ref contract_call) => {
            let formatted_args = contract_call
                .function_args
                .iter()
                .map(|v| format!("{}", v))
                .collect::<Vec<String>>();
            (
                format!(
                    "invoked: {}.{}::{}({})",
                    contract_call.address,
                    contract_call.contract_name,
                    contract_call.function_name,
                    formatted_args.join(", ")
                ),
                StacksTransactionKind::ContractCall(StacksContractCallData {
                    contract_identifier: format!(
                        "{}.{}",
                        contract_call.address, contract_call.contract_name
                    ),
                    method: contract_call.function_name.to_string(),
                    args: formatted_args,
                }),
            )
        }
        TransactionPayload::SmartContract(ref smart_contract) => {
            let contract_identifier = format!("{}.{}", tx.origin_address(), smart_contract.name);
            let data = StacksContractDeploymentData {
                contract_identifier: contract_identifier.clone(),
                code: smart_contract.code_body.to_string(),
            };
            (
                format!("deployed: {}", contract_identifier),
                StacksTransactionKind::ContractDeployment(data),
            )
        }
        TransactionPayload::Coinbase(..) => (format!("coinbase"), StacksTransactionKind::Coinbase),
        _ => (format!("other"), StacksTransactionKind::Other),
    };
    Ok((description, tx_type, fee, sender, sponsor))
}

pub fn get_standardized_fungible_currency_from_asset_class_id(
    asset_class_id: &str,
    asset_class_cache: &mut HashMap<String, AssetClassCache>,
    _node_url: &str,
) -> Currency {
    match asset_class_cache.get(asset_class_id) {
        None => {
            // TODO(lgalabru): re-approach this, with an adequate runtime strategy.
            // let comps = asset_class_id.split("::").collect::<Vec<&str>>();
            // let principal = comps[0].split(".").collect::<Vec<&str>>();
            // let contract_address = principal[0];
            // let contract_name = principal[1];
            // let stacks_rpc = StacksRpc::new(&node_url);
            // let value = stacks_rpc
            //     .call_read_only_fn(
            //         &contract_address,
            //         &contract_name,
            //         "get-symbol",
            //         vec![],
            //         contract_address,
            //     )
            //     .expect("Unable to retrieve symbol");
            let symbol = "TOKEN".into(); //value.expect_result_ok().expect_ascii();

            // let value = stacks_rpc
            //     .call_read_only_fn(
            //         &contract_address,
            //         &contract_name,
            //         "get-decimals",
            //         vec![],
            //         &contract_address,
            //     )
            //     .expect("Unable to retrieve decimals");
            let decimals = 6; // value.expect_result_ok().expect_u128() as u8;

            let entry = AssetClassCache { symbol, decimals };

            let currency = Currency {
                symbol: entry.symbol.clone(),
                decimals: entry.decimals.into(),
                metadata: Some(CurrencyMetadata {
                    asset_class_identifier: asset_class_id.into(),
                    asset_identifier: None,
                    standard: CurrencyStandard::Sip10,
                }),
            };

            asset_class_cache.insert(asset_class_id.into(), entry);

            currency
        }
        Some(entry) => Currency {
            symbol: entry.symbol.clone(),
            decimals: entry.decimals.into(),
            metadata: Some(CurrencyMetadata {
                asset_class_identifier: asset_class_id.into(),
                asset_identifier: None,
                standard: CurrencyStandard::Sip10,
            }),
        },
    }
}

pub fn get_standardized_non_fungible_currency_from_asset_class_id(
    asset_class_id: &str,
    asset_id: &str,
    _asset_class_cache: &mut HashMap<String, AssetClassCache>,
) -> Currency {
    Currency {
        symbol: asset_class_id.into(),
        decimals: 0,
        metadata: Some(CurrencyMetadata {
            asset_class_identifier: asset_class_id.into(),
            asset_identifier: Some(asset_id.into()),
            standard: CurrencyStandard::Sip09,
        }),
    }
}

pub fn get_standardized_stacks_operations(
    txid: &str,
    events: &mut Vec<NewEvent>,
    asset_class_cache: &mut HashMap<String, AssetClassCache>,
    node_url: &str,
) -> (Vec<Operation>, StacksTransactionReceipt) {
    let mut mutated_contracts_radius = HashSet::new();
    let mut mutated_assets_radius = HashSet::new();
    let mut marshalled_events = Vec::new();

    let mut operations = vec![];
    let mut operation_id = 0;

    let mut i = 0;
    while i < events.len() {
        if events[i].txid == txid {
            let event = events.remove(i);
            if let Some(ref event_data) = event.stx_mint_event {
                let data: STXMintEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::STXMintEvent(data.clone()));
                operations.push(Operation {
                    operation_identifier: OperationIdentifier {
                        index: operation_id,
                        network_index: None,
                    },
                    related_operations: None,
                    type_: OperationType::Credit,
                    status: Some(OperationStatusKind::Success),
                    account: AccountIdentifier {
                        address: data.recipient,
                        sub_account: None,
                    },
                    amount: Some(Amount {
                        value: data.amount.parse::<u64>().expect("Unable to parse u64"),
                        currency: get_stacks_currency(),
                    }),
                    metadata: None,
                });
                operation_id += 1;
            } else if let Some(ref event_data) = event.stx_lock_event {
                let data: STXLockEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::STXLockEvent(data.clone()));
                operations.push(Operation {
                    operation_identifier: OperationIdentifier {
                        index: operation_id,
                        network_index: None,
                    },
                    related_operations: None,
                    type_: OperationType::Lock,
                    status: Some(OperationStatusKind::Success),
                    account: AccountIdentifier {
                        address: data.locked_address,
                        sub_account: None,
                    },
                    amount: Some(Amount {
                        value: data
                            .locked_amount
                            .parse::<u64>()
                            .expect("Unable to parse u64"),
                        currency: get_stacks_currency(),
                    }),
                    metadata: None,
                });
                operation_id += 1;
            } else if let Some(ref event_data) = event.stx_burn_event {
                let data: STXBurnEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::STXBurnEvent(data.clone()));
                operations.push(Operation {
                    operation_identifier: OperationIdentifier {
                        index: operation_id,
                        network_index: None,
                    },
                    related_operations: None,
                    type_: OperationType::Debit,
                    status: Some(OperationStatusKind::Success),
                    account: AccountIdentifier {
                        address: data.sender,
                        sub_account: None,
                    },
                    amount: Some(Amount {
                        value: data.amount.parse::<u64>().expect("Unable to parse u64"),
                        currency: get_stacks_currency(),
                    }),
                    metadata: None,
                });
                operation_id += 1;
            } else if let Some(ref event_data) = event.stx_transfer_event {
                let data: STXTransferEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::STXTransferEvent(data.clone()));
                operations.push(Operation {
                    operation_identifier: OperationIdentifier {
                        index: operation_id,
                        network_index: None,
                    },
                    related_operations: Some(vec![OperationIdentifier {
                        index: operation_id + 1,
                        network_index: None,
                    }]),
                    type_: OperationType::Debit,
                    status: Some(OperationStatusKind::Success),
                    account: AccountIdentifier {
                        address: data.sender,
                        sub_account: None,
                    },
                    amount: Some(Amount {
                        value: data.amount.parse::<u64>().expect("Unable to parse u64"),
                        currency: get_stacks_currency(),
                    }),
                    metadata: None,
                });
                operation_id += 1;
                operations.push(Operation {
                    operation_identifier: OperationIdentifier {
                        index: operation_id,
                        network_index: None,
                    },
                    related_operations: Some(vec![OperationIdentifier {
                        index: operation_id - 1,
                        network_index: None,
                    }]),
                    type_: OperationType::Credit,
                    status: Some(OperationStatusKind::Success),
                    account: AccountIdentifier {
                        address: data.recipient,
                        sub_account: None,
                    },
                    amount: Some(Amount {
                        value: data.amount.parse::<u64>().expect("Unable to parse u64"),
                        currency: get_stacks_currency(),
                    }),
                    metadata: None,
                });
                operation_id += 1;
            } else if let Some(ref event_data) = event.nft_mint_event {
                let data: NFTMintEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::NFTMintEvent(data.clone()));
                let (asset_class_identifier, contract_identifier) =
                    get_mutated_ids(&data.asset_class_identifier);
                mutated_assets_radius.insert(asset_class_identifier);
                mutated_contracts_radius.insert(contract_identifier);

                let currency = get_standardized_non_fungible_currency_from_asset_class_id(
                    &data.asset_class_identifier,
                    &data.hex_asset_identifier,
                    asset_class_cache,
                );
                operations.push(Operation {
                    operation_identifier: OperationIdentifier {
                        index: operation_id,
                        network_index: None,
                    },
                    related_operations: None,
                    type_: OperationType::Credit,
                    status: Some(OperationStatusKind::Success),
                    account: AccountIdentifier {
                        address: data.recipient,
                        sub_account: None,
                    },
                    amount: Some(Amount { value: 1, currency }),
                    metadata: None,
                });
                operation_id += 1;
            } else if let Some(ref event_data) = event.nft_burn_event {
                let data: NFTBurnEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::NFTBurnEvent(data.clone()));
                let (asset_class_identifier, contract_identifier) =
                    get_mutated_ids(&data.asset_class_identifier);
                mutated_assets_radius.insert(asset_class_identifier);
                mutated_contracts_radius.insert(contract_identifier);

                let currency = get_standardized_non_fungible_currency_from_asset_class_id(
                    &data.asset_class_identifier,
                    &data.hex_asset_identifier,
                    asset_class_cache,
                );
                operations.push(Operation {
                    operation_identifier: OperationIdentifier {
                        index: operation_id,
                        network_index: None,
                    },
                    related_operations: None,
                    type_: OperationType::Debit,
                    status: Some(OperationStatusKind::Success),
                    account: AccountIdentifier {
                        address: data.sender,
                        sub_account: None,
                    },
                    amount: Some(Amount { value: 1, currency }),
                    metadata: None,
                });
                operation_id += 1;
            } else if let Some(ref event_data) = event.nft_transfer_event {
                let data: NFTTransferEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::NFTTransferEvent(data.clone()));
                let (asset_class_identifier, contract_identifier) =
                    get_mutated_ids(&data.asset_class_identifier);
                mutated_assets_radius.insert(asset_class_identifier);
                mutated_contracts_radius.insert(contract_identifier);

                let currency = get_standardized_non_fungible_currency_from_asset_class_id(
                    &data.asset_class_identifier,
                    &data.hex_asset_identifier,
                    asset_class_cache,
                );
                operations.push(Operation {
                    operation_identifier: OperationIdentifier {
                        index: operation_id,
                        network_index: None,
                    },
                    related_operations: Some(vec![OperationIdentifier {
                        index: operation_id + 1,
                        network_index: None,
                    }]),
                    type_: OperationType::Debit,
                    status: Some(OperationStatusKind::Success),
                    account: AccountIdentifier {
                        address: data.sender,
                        sub_account: None,
                    },
                    amount: Some(Amount {
                        value: 1,
                        currency: currency.clone(),
                    }),
                    metadata: None,
                });
                operation_id += 1;
                operations.push(Operation {
                    operation_identifier: OperationIdentifier {
                        index: operation_id,
                        network_index: None,
                    },
                    related_operations: Some(vec![OperationIdentifier {
                        index: operation_id - 1,
                        network_index: None,
                    }]),
                    type_: OperationType::Credit,
                    status: Some(OperationStatusKind::Success),
                    account: AccountIdentifier {
                        address: data.recipient,
                        sub_account: None,
                    },
                    amount: Some(Amount { value: 1, currency }),
                    metadata: None,
                });
                operation_id += 1;
            } else if let Some(ref event_data) = event.ft_mint_event {
                let data: FTMintEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::FTMintEvent(data.clone()));
                let (asset_class_identifier, contract_identifier) =
                    get_mutated_ids(&data.asset_class_identifier);
                mutated_assets_radius.insert(asset_class_identifier);
                mutated_contracts_radius.insert(contract_identifier);

                let currency = get_standardized_fungible_currency_from_asset_class_id(
                    &data.asset_class_identifier,
                    asset_class_cache,
                    node_url,
                );
                operations.push(Operation {
                    operation_identifier: OperationIdentifier {
                        index: operation_id,
                        network_index: None,
                    },
                    related_operations: None,
                    type_: OperationType::Credit,
                    status: Some(OperationStatusKind::Success),
                    account: AccountIdentifier {
                        address: data.recipient,
                        sub_account: None,
                    },
                    amount: Some(Amount {
                        value: data.amount.parse::<u64>().expect("Unable to parse u64"),
                        currency,
                    }),
                    metadata: None,
                });
                operation_id += 1;
            } else if let Some(ref event_data) = event.ft_burn_event {
                let data: FTBurnEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::FTBurnEvent(data.clone()));
                let (asset_class_identifier, contract_identifier) =
                    get_mutated_ids(&data.asset_class_identifier);
                mutated_assets_radius.insert(asset_class_identifier);
                mutated_contracts_radius.insert(contract_identifier);

                let currency = get_standardized_fungible_currency_from_asset_class_id(
                    &data.asset_class_identifier,
                    asset_class_cache,
                    node_url,
                );
                operations.push(Operation {
                    operation_identifier: OperationIdentifier {
                        index: operation_id,
                        network_index: None,
                    },
                    related_operations: None,
                    type_: OperationType::Debit,
                    status: Some(OperationStatusKind::Success),
                    account: AccountIdentifier {
                        address: data.sender,
                        sub_account: None,
                    },
                    amount: Some(Amount {
                        value: data.amount.parse::<u64>().expect("Unable to parse u64"),
                        currency,
                    }),
                    metadata: None,
                });
                operation_id += 1;
            } else if let Some(ref event_data) = event.ft_transfer_event {
                let data: FTTransferEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::FTTransferEvent(data.clone()));
                let (asset_class_identifier, contract_identifier) =
                    get_mutated_ids(&data.asset_class_identifier);
                mutated_assets_radius.insert(asset_class_identifier);
                mutated_contracts_radius.insert(contract_identifier);

                let currency = get_standardized_fungible_currency_from_asset_class_id(
                    &data.asset_class_identifier,
                    asset_class_cache,
                    node_url,
                );
                operations.push(Operation {
                    operation_identifier: OperationIdentifier {
                        index: operation_id,
                        network_index: None,
                    },
                    related_operations: Some(vec![OperationIdentifier {
                        index: operation_id + 1,
                        network_index: None,
                    }]),
                    type_: OperationType::Debit,
                    status: Some(OperationStatusKind::Success),
                    account: AccountIdentifier {
                        address: data.sender,
                        sub_account: None,
                    },
                    amount: Some(Amount {
                        value: data.amount.parse::<u64>().expect("Unable to parse u64"),
                        currency: currency.clone(),
                    }),
                    metadata: None,
                });
                operation_id += 1;
                operations.push(Operation {
                    operation_identifier: OperationIdentifier {
                        index: operation_id,
                        network_index: None,
                    },
                    related_operations: Some(vec![OperationIdentifier {
                        index: operation_id - 1,
                        network_index: None,
                    }]),
                    type_: OperationType::Credit,
                    status: Some(OperationStatusKind::Success),
                    account: AccountIdentifier {
                        address: data.recipient,
                        sub_account: None,
                    },
                    amount: Some(Amount {
                        value: data.amount.parse::<u64>().expect("Unable to parse u64"),
                        currency,
                    }),
                    metadata: None,
                });
                operation_id += 1;
            } else if let Some(ref event_data) = event.data_var_set_event {
                let data: DataVarSetEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::DataVarSetEvent(data.clone()));
                mutated_contracts_radius.insert(data.contract_identifier.clone());
            } else if let Some(ref event_data) = event.data_map_insert_event {
                let data: DataMapInsertEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::DataMapInsertEvent(data.clone()));
                mutated_contracts_radius.insert(data.contract_identifier.clone());
            } else if let Some(ref event_data) = event.data_map_update_event {
                let data: DataMapUpdateEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::DataMapUpdateEvent(data.clone()));
                mutated_contracts_radius.insert(data.contract_identifier.clone());
            } else if let Some(ref event_data) = event.data_map_delete_event {
                let data: DataMapDeleteEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::DataMapDeleteEvent(data.clone()));
                mutated_contracts_radius.insert(data.contract_identifier.clone());
            } else if let Some(ref event_data) = event.contract_event {
                let data: SmartContractEventData = serde_json::from_value(event_data.clone())
                    .expect("Unable to decode event_data");
                marshalled_events.push(StacksTransactionEvent::SmartContractEvent(data.clone()));
                mutated_contracts_radius.insert(data.contract_identifier.clone());
            }
        } else {
            i += 1;
        }
    }
    let receipt = StacksTransactionReceipt::new(
        mutated_contracts_radius,
        mutated_assets_radius,
        marshalled_events,
    );
    (operations, receipt)
}

fn get_mutated_ids(asset_class_id: &str) -> (String, String) {
    let contract_id = asset_class_id.split("::").collect::<Vec<_>>()[0];
    (asset_class_id.into(), contract_id.into())
}

#[cfg(test)]
pub mod tests;
