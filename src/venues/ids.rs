use solana_sdk::pubkey::Pubkey;
use std::sync::LazyLock;

pub static PUMP_PROGRAM: LazyLock<Pubkey> = LazyLock::new(|| pubkey("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"));
pub static PUMP_AMM_PROGRAM: LazyLock<Pubkey> = LazyLock::new(|| pubkey("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"));
pub static PUMP_FEE_PROGRAM: LazyLock<Pubkey> = LazyLock::new(|| pubkey("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ"));
pub static TOKEN_PROGRAM: LazyLock<Pubkey> = LazyLock::new(|| pubkey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"));
pub static TOKEN_2022_PROGRAM: LazyLock<Pubkey> = LazyLock::new(|| pubkey("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"));
pub static ATA_PROGRAM: LazyLock<Pubkey> = LazyLock::new(|| pubkey("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"));
pub static WSOL_MINT: LazyLock<Pubkey> = LazyLock::new(|| pubkey("So11111111111111111111111111111111111111112"));

pub static PUMP_FEE_RECIPIENTS: LazyLock<Vec<Pubkey>> = LazyLock::new(|| {
    vec![
        "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM",
        "62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV",
        "FWsW1xNtWscwNmKv6wVsU1iTzRN6wmmk3MjxRP5tT7hz",
        "7hTckgnGnLQR6sdH7YkqFTAA7VwTfYFaZ6EhEsU3saCX",
        "AVmoTthdrX6tKt4nDjco2D775W2YK3sDhxPcMmzUAmTY",
        "9rPYyANsfQZw3DnDmKE3YCQF5E8oD89UXoHn9JFEhJUz",
        "G5UZAVbAf46s7cKWoyKu8kYTip9DGTpbLZ2qa9Aq69dP",
        "7VtfL8fvgNfhz17qKRMjzQEXgbdpnHHHQRh54R9jP2RJ",
    ]
    .into_iter()
    .map(pubkey)
    .collect()
});

pub static PUMP_BUYBACK_FEE_RECIPIENTS: LazyLock<Vec<Pubkey>> = LazyLock::new(|| {
    vec![
        "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD",
        "9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7",
        "GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL",
        "3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR",
        "5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6",
        "EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL",
        "5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD",
        "A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW",
    ]
    .into_iter()
    .map(pubkey)
    .collect()
});

pub static HELIUS_TIP_ACCOUNTS: LazyLock<Vec<Pubkey>> = LazyLock::new(|| {
    vec![
        "4ACfpUFoaSD9bfPdeu6DBt89gB6ENTeHBXCAi87NhDEE",
        "D2L6yPZ2FmmmTKPgzaMKdhu6EWZcTpLy1Vhx8uvZe7NZ",
        "9bnz4RShgq1hAnLnZbP8kbgBg1kEmcJBYQq3gQbmnSta",
    ]
    .into_iter()
    .map(pubkey)
    .collect()
});

pub fn pubkey(value: &str) -> Pubkey {
    value.parse().expect("hard-coded pubkey")
}

pub fn pda(program: &Pubkey, seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}

pub fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    pda(&ATA_PROGRAM, &[owner.as_ref(), token_program.as_ref(), mint.as_ref()])
}

pub fn bonding_curve_pda(mint: &Pubkey) -> Pubkey {
    pda(&PUMP_PROGRAM, &[b"bonding-curve", mint.as_ref()])
}

pub fn bonding_curve_v2_pda(mint: &Pubkey) -> Pubkey {
    pda(&PUMP_PROGRAM, &[b"bonding-curve-v2", mint.as_ref()])
}

pub fn creator_vault_pda(creator: &Pubkey) -> Pubkey {
    pda(&PUMP_PROGRAM, &[b"creator-vault", creator.as_ref()])
}

pub fn pump_global() -> Pubkey {
    pda(&PUMP_PROGRAM, &[b"global"])
}

pub fn pump_event_authority() -> Pubkey {
    pda(&PUMP_PROGRAM, &[b"__event_authority"])
}

pub fn pump_fee_config() -> Pubkey {
    pda(&PUMP_FEE_PROGRAM, &[b"fee_config", PUMP_PROGRAM.as_ref()])
}

pub fn pump_global_volume() -> Pubkey {
    pda(&PUMP_PROGRAM, &[b"global_volume_accumulator"])
}

pub fn pump_user_volume(user: &Pubkey) -> Pubkey {
    pda(&PUMP_PROGRAM, &[b"user_volume_accumulator", user.as_ref()])
}

pub fn amm_global_config() -> Pubkey {
    pda(&PUMP_AMM_PROGRAM, &[b"global_config"])
}

pub fn amm_event_authority() -> Pubkey {
    pda(&PUMP_AMM_PROGRAM, &[b"__event_authority"])
}

pub fn amm_global_volume() -> Pubkey {
    pda(&PUMP_AMM_PROGRAM, &[b"global_volume_accumulator"])
}

pub fn amm_user_volume(user: &Pubkey) -> Pubkey {
    pda(&PUMP_AMM_PROGRAM, &[b"user_volume_accumulator", user.as_ref()])
}

pub fn amm_fee_config() -> Pubkey {
    pda(&PUMP_FEE_PROGRAM, &[b"fee_config", PUMP_AMM_PROGRAM.as_ref()])
}

pub fn coin_creator_vault_authority(coin_creator: &Pubkey) -> Pubkey {
    pda(&PUMP_AMM_PROGRAM, &[b"creator_vault", coin_creator.as_ref()])
}

pub fn pool_v2_pda(base_mint: &Pubkey) -> Pubkey {
    pda(&PUMP_AMM_PROGRAM, &[b"pool-v2", base_mint.as_ref()])
}

pub fn pump_pool_authority(mint: &Pubkey) -> Pubkey {
    pda(&PUMP_PROGRAM, &[b"pool-authority", mint.as_ref()])
}

pub fn canonical_pool_pda(mint: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
    let index_bytes = 0u16.to_le_bytes();
    pda(
        &PUMP_AMM_PROGRAM,
        &[b"pool", &index_bytes, pump_pool_authority(mint).as_ref(), mint.as_ref(), quote_mint.as_ref()],
    )
}

/// On-chain PumpSwap `Pool` account (disc stripped layout).
#[derive(Debug, Clone)]
pub struct AmmPoolAccount {
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    pub coin_creator: Pubkey,
    pub cashback: bool,
    pub virtual_quote_reserves: i128,
}

pub fn parse_amm_pool(data: &[u8]) -> Option<AmmPoolAccount> {
    // body offsets: bump@0, index@1, creator@3, base@35, quote@67, lp@99,
    // pool_base@131, pool_quote@163, lp_supply@195, coin_creator@203, mayhem@235, cashback@236
    const MIN: usize = 8 + 237;
    if data.len() < MIN {
        return None;
    }
    let body = &data[8..];
    let read_pk = |at: usize| -> Option<Pubkey> {
        Some(Pubkey::new_from_array(body.get(at..at + 32)?.try_into().ok()?))
    };
    Some(AmmPoolAccount {
        base_mint: read_pk(35)?,
        quote_mint: read_pk(67)?,
        pool_base_token_account: read_pk(131)?,
        pool_quote_token_account: read_pk(163)?,
        coin_creator: read_pk(203)?,
        cashback: body.get(236).copied().unwrap_or(0) != 0,
        virtual_quote_reserves: body
            .get(237..253)
            .and_then(|b| b.try_into().ok())
            .map(i128::from_le_bytes)
            .unwrap_or(0),
    })
}

#[derive(Debug, Clone)]
pub struct AmmGlobalConfig {
    pub lp_fee_bps: u64,
    pub protocol_fee_bps: u64,
    pub protocol_fee_recipients: [Pubkey; 8],
    pub coin_creator_fee_bps: u64,
}

pub fn parse_amm_global_config(data: &[u8]) -> Option<AmmGlobalConfig> {
    // disc(8) + admin(32) + lp(8) + protocol(8) + disable(1) + recipients(8*32) + creator_fee(8)
    const MIN: usize = 8 + 32 + 8 + 8 + 1 + 32 * 8 + 8;
    if data.len() < MIN {
        return None;
    }
    let body = &data[8..];
    let lp_fee_bps = u64::from_le_bytes(body[32..40].try_into().ok()?);
    let protocol_fee_bps = u64::from_le_bytes(body[40..48].try_into().ok()?);
    let mut protocol_fee_recipients = [Pubkey::default(); 8];
    for (i, slot) in protocol_fee_recipients.iter_mut().enumerate() {
        let at = 49 + i * 32;
        *slot = Pubkey::new_from_array(body.get(at..at + 32)?.try_into().ok()?);
    }
    let coin_creator_fee_bps = u64::from_le_bytes(body[49 + 256..49 + 256 + 8].try_into().ok()?);
    Some(AmmGlobalConfig {
        lp_fee_bps,
        protocol_fee_bps,
        protocol_fee_recipients,
        coin_creator_fee_bps,
    })
}
