//! Yellowstone subscribe request — same shape as New Folder.
//! Target wallet(s) + own wallet(s) in tx `account_include`, then per-mint
//! pools after warm-up. Reserve accounts stream separately for live curve/vault state.

use std::collections::HashMap;

use solana_sdk::pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::{
    CommitmentLevel, SubscribeRequest, SubscribeRequestFilterAccounts,
    SubscribeRequestFilterTransactions,
};

use crate::venues::ids::PUMP_PROGRAM;
use crate::venues::launchpad::{PLATFORM_REWARD, PLATFORM_STANDARD};

#[derive(Debug, Clone, Default)]
pub struct SubscriptionState {
    /// mint → bonding_curve or AMM pool PDA (tx filter include).
    pub watched_pools: HashMap<Pubkey, Pubkey>,
    /// mint → reserve accounts to stream.
    pub watched_accounts: HashMap<Pubkey, Vec<Pubkey>>,
    /// Bot signing wallets — confirm / ATA echo.
    pub wallet_pubkeys: Vec<Pubkey>,
    /// Target / gate wallets in the tx filter.
    pub target_wallets: Vec<Pubkey>,
    /// Pump.fun plus the two Stonk.fun platforms, for v071 age and volume.
    pub pump_firehose: bool,
}

pub fn build_request(state: &SubscriptionState) -> SubscribeRequest {
    let account_include: Vec<String> = state
        .watched_pools
        .values()
        .map(|p| p.to_string())
        .chain(state.wallet_pubkeys.iter().map(|p| p.to_string()))
        .chain(state.target_wallets.iter().map(|p| p.to_string()))
        .collect();

    let mut transactions = HashMap::new();
    if !account_include.is_empty() {
        transactions.insert(
            "mint_pools".to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                account_include,
                ..Default::default()
            },
        );
    }
    if state.pump_firehose {
        transactions.insert(
            "pump_firehose".to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                account_include: vec![
                    PUMP_PROGRAM.to_string(),
                    PLATFORM_REWARD.to_string(),
                    PLATFORM_STANDARD.to_string(),
                ],
                ..Default::default()
            },
        );
    }

    let watched_account_list: Vec<String> = state
        .watched_accounts
        .values()
        .flatten()
        .map(|p| p.to_string())
        .collect();

    let mut accounts = HashMap::new();
    if !watched_account_list.is_empty() {
        accounts.insert(
            "watch_account_state".to_string(),
            SubscribeRequestFilterAccounts {
                account: watched_account_list,
                ..Default::default()
            },
        );
    }

    SubscribeRequest {
        accounts,
        transactions,
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    }
}
