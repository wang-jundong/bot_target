//! Non-SOL pump curve path.
//!
//! Buy is one transaction: Jupiter `SOL → quote`, then pump `buy_exact_quote_in_v2`
//! spending Jupiter's minimum output. If that transaction is too large, the two
//! legs are sent separately. A live-curve sell is mint → quote only. The quote
//! is swapped back to SOL after the position is flat.

use crate::venues::ids::{self, ata};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::Value;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::AddressLookupTableAccount;
use solana_sdk::pubkey::Pubkey;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const JUPITER_PRICE: &str = "https://api.jup.ag/price/v3";
const JUPITER_BUILD: &str = "https://api.jup.ag/swap/v2/build";
const WSOL: &str = "So11111111111111111111111111111111111111112";
/// 50% slippage, so the curve spends the minimum the SOL→quote hop can still deliver.
pub const JUPITER_BUY_SLIPPAGE_BPS: u16 = 5_000;
/// 1% slippage when swapping leftover quote back to SOL.
pub const JUPITER_RECLAIM_SLIPPAGE_BPS: u16 = 100;
/// 100% slippage when the curve has migrated and the bag is sold through Jupiter.
pub const JUPITER_FLATTEN_SLIPPAGE_BPS: u16 = 10_000;
/// Wait after a flat sell before swapping quote back to SOL.
pub const RECLAIM_DELAY: Duration = Duration::from_secs(2);
/// Second reclaim after a mint is dropped with no position still open.
pub const ABANDON_RECLAIM_DELAY: Duration = Duration::from_secs(8);
/// A SOL→quote hop plus the curve does not fit in 150k compute units.
pub const ALT_COMPUTE_UNIT_LIMIT: u32 = 600_000;
const FX_MAX_AGE: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct FxEntry {
    sol_per_quote: f64,
    at: Instant,
}

#[derive(Clone)]
struct QuoteMeta {
    decimals: u8,
    token_program: Pubkey,
}

pub struct JupiterBuild {
    pub instructions: Vec<Instruction>,
    pub alts: Vec<AddressLookupTableAccount>,
    pub out_amount: u64,
    pub other_amount_threshold: u64,
}

pub struct AltBuyLegs {
    pub mint: Pubkey,
    pub token_program: Pubkey,
    pub quote_mint: Pubkey,
    pub quote_program: Pubkey,
    pub quote_atoms: u64,
    pub curve_slippage_bps: u64,
    pub jupiter: JupiterBuild,
    pub curve: Vec<Instruction>,
    pub compute_unit_limit: u32,
    /// Curve retries rebuild LaunchLab `buy_exact_in` instead of pump `buy_exact_quote_in_v2`.
    pub launchpad: bool,
    /// Tokens the curve math expected before slippage. Used when the token-account read fails.
    pub expected_tokens: u64,
}

pub struct AltQuote {
    http: reqwest::Client,
    rpc: std::sync::Arc<RpcClient>,
    fx: Mutex<HashMap<Pubkey, FxEntry>>,
    fx_refreshing: Mutex<HashSet<Pubkey>>,
    meta: Mutex<HashMap<Pubkey, QuoteMeta>>,
    meta_refreshing: Mutex<HashSet<Pubkey>>,
}

impl AltQuote {
    pub fn new(rpc: std::sync::Arc<RpcClient>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(12))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            rpc,
            fx: Mutex::new(HashMap::new()),
            fx_refreshing: Mutex::new(HashSet::new()),
            meta: Mutex::new(HashMap::new()),
            meta_refreshing: Mutex::new(HashSet::new()),
        }
    }

    pub async fn token_program(&self, quote_mint: &Pubkey) -> Result<Pubkey> {
        Ok(self.ensure_meta(quote_mint).await?.token_program)
    }

    /// Cached mint decimals / token program. Missing means a background fetch
    /// is needed; the detect loop must not await that RPC.
    fn cached_meta(&self, quote_mint: &Pubkey) -> Option<QuoteMeta> {
        self.meta.lock().expect("meta").get(quote_mint).cloned()
    }

    /// Fresh cached SOL-per-quote. A missing or stale rate is `None`; the caller
    /// keeps the print and refreshes in the background.
    pub fn fresh_sol_per_quote(&self, quote_mint: &Pubkey) -> Option<f64> {
        let fx = self.fx.lock().expect("fx");
        let entry = fx.get(quote_mint)?;
        if entry.at.elapsed() <= FX_MAX_AGE && entry.sol_per_quote > 0.0 {
            Some(entry.sol_per_quote)
        } else {
            None
        }
    }

    /// One Jupiter price fetch per quote while a refresh is already running.
    pub fn spawn_fx_refresh(self: &Arc<Self>, quote_mint: Pubkey) {
        {
            let mut refreshing = self.fx_refreshing.lock().expect("fx refresh");
            if !refreshing.insert(quote_mint) {
                return;
            }
        }
        let quotes = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = quotes.sol_per_quote(&quote_mint).await {
                tracing::warn!(
                    error = %err,
                    quote = %quote_mint,
                    "[03 DETECT] Quote FX refresh failed"
                );
            }
            quotes
                .fx_refreshing
                .lock()
                .expect("fx refresh")
                .remove(&quote_mint);
        });
    }

    /// One mint-account fetch per quote while a refresh is already running.
    pub fn spawn_meta_refresh(self: &Arc<Self>, quote_mint: Pubkey) {
        {
            let mut refreshing = self.meta_refreshing.lock().expect("meta refresh");
            if !refreshing.insert(quote_mint) {
                return;
            }
        }
        let quotes = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = quotes.ensure_meta(&quote_mint).await {
                tracing::warn!(
                    error = %err,
                    quote = %quote_mint,
                    "[03 DETECT] Quote meta refresh failed"
                );
            }
            quotes
                .meta_refreshing
                .lock()
                .expect("meta refresh")
                .remove(&quote_mint);
        });
    }

    /// Prefetch FX + mint meta off the detect loop (gate buy / first print).
    pub fn spawn_quote_ready(self: &Arc<Self>, quote_mint: Pubkey) {
        self.spawn_fx_refresh(quote_mint);
        self.spawn_meta_refresh(quote_mint);
    }

    /// Scale from the cached rate + mint meta only. `false` means FX or meta is
    /// missing; the caller keeps the print and refreshes in the background.
    /// Never RPCs — safe on the serial inbound handler.
    pub fn scale_from_cache(
        &self,
        quote_mint: &Pubkey,
        atoms: &mut u64,
        price: &mut f64,
    ) -> bool {
        let Some(fx) = self.fresh_sol_per_quote(quote_mint) else {
            return false;
        };
        let Some(meta) = self.cached_meta(quote_mint) else {
            return false;
        };
        *atoms = lamports_from_quote_atoms(*atoms, fx, meta.decimals);
        *price = scale_price(*price, fx, meta.decimals);
        true
    }

    /// Quote atoms as SOL from cache only. `None` means FX/meta is missing.
    pub fn quote_atoms_to_sol_cached(&self, quote_mint: &Pubkey, atoms: u64) -> Option<f64> {
        let fx = self.fresh_sol_per_quote(quote_mint)?;
        let meta = self.cached_meta(quote_mint)?;
        Some(lamports_from_quote_atoms(atoms, fx, meta.decimals) as f64 / 1_000_000_000.0)
    }

    /// Quote atoms as SOL, using the cached Jupiter rate or fetching one.
    pub async fn quote_atoms_to_sol(&self, quote_mint: &Pubkey, atoms: u64) -> Result<f64> {
        let meta = self.ensure_meta(quote_mint).await?;
        let fx = self.sol_per_quote(quote_mint).await?;
        Ok(lamports_from_quote_atoms(atoms, fx, meta.decimals) as f64 / 1_000_000_000.0)
    }

    pub async fn quote_fill_to_sol(
        &self,
        quote_mint: &Pubkey,
        quote_atoms: u64,
        price: f64,
    ) -> Result<(u64, f64)> {
        let meta = self.ensure_meta(quote_mint).await?;
        let fx = self.sol_per_quote(quote_mint).await?;
        Ok((
            lamports_from_quote_atoms(quote_atoms, fx, meta.decimals),
            scale_price(price, fx, meta.decimals),
        ))
    }

    pub async fn quote_balance(&self, owner: &Pubkey, quote_mint: &Pubkey) -> Result<u64> {
        let meta = self.ensure_meta(quote_mint).await?;
        let address = ata(owner, quote_mint, &meta.token_program);
        match self.rpc.get_account(&address).await {
            Ok(account) if account.data.len() >= 72 => Ok(u64::from_le_bytes(
                account.data[64..72].try_into().expect("8 bytes"),
            )),
            _ => Ok(0),
        }
    }

    /// Jupiter SOL→quote hop plus the curve instructions that spend its min-out.
    pub async fn buy_legs(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
        token_program: &Pubkey,
        quote_mint: &Pubkey,
        spend_lamports: u64,
        curve_slippage_bps: u64,
        curve: &crate::events::PumpCurveSnapshot,
        compute_unit_limit: u32,
    ) -> Result<AltBuyLegs> {
        let (jupiter, quote_program, quote_atoms) = self
            .funded_quote(owner, quote_mint, spend_lamports)
            .await?;
        let expected_tokens =
            crate::venues::quote::quote_buy_tokens(quote_atoms as u128, curve) as u64;
        let curve_ixs = crate::venues::trade::pump_alt_buy_instructions(
            owner,
            mint,
            token_program,
            quote_mint,
            &quote_program,
            quote_atoms,
            curve_slippage_bps,
            curve,
        )?;
        Ok(AltBuyLegs {
            mint: *mint,
            token_program: *token_program,
            quote_mint: *quote_mint,
            quote_program,
            quote_atoms,
            curve_slippage_bps,
            jupiter,
            curve: curve_ixs,
            compute_unit_limit: compute_unit_limit.max(ALT_COMPUTE_UNIT_LIMIT),
            launchpad: false,
            expected_tokens,
        })
    }

    /// Balance of `owner`'s token account. Missing or unreadable accounts are errors
    /// so the caller can fall back to the quoted token amount.
    pub async fn token_account_amount(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
        token_program: &Pubkey,
    ) -> Result<u64> {
        let address = ata(owner, mint, token_program);
        let account = self
            .rpc
            .get_account(&address)
            .await
            .with_context(|| format!("token account {address} missing"))?;
        if account.data.len() < 72 {
            bail!("token account {address} is too short");
        }
        Ok(u64::from_le_bytes(account.data[64..72].try_into()?))
    }

    /// SOL→quote hop only. The caller attaches pump or LaunchLab spend instructions.
    pub async fn funded_quote(
        &self,
        owner: &Pubkey,
        quote_mint: &Pubkey,
        spend_lamports: u64,
    ) -> Result<(JupiterBuild, Pubkey, u64)> {
        if spend_lamports == 0 {
            bail!("alt-quote buy size is 0");
        }
        if ids::is_sol_quote(Some(&quote_mint.to_string())) {
            bail!("alt-quote buy requires a non-SOL quote");
        }
        let quote_program = self.token_program(quote_mint).await?;
        let jupiter = self
            .jupiter_sol_to_quote(owner, quote_mint, spend_lamports)
            .await?;
        let quote_atoms = if jupiter.other_amount_threshold > 0 {
            jupiter.other_amount_threshold
        } else {
            jupiter
                .out_amount
                .saturating_mul(10_000u64.saturating_sub(JUPITER_BUY_SLIPPAGE_BPS as u64))
                / 10_000
        }
        .max(1);
        Ok((jupiter, quote_program, quote_atoms))
    }

    /// SOL-lamport value of the quote atoms the curve was told to spend.
    pub async fn curve_spend_lamports(
        &self,
        quote_mint: &Pubkey,
        quote_atoms: u64,
    ) -> Result<u64> {
        let meta = self.ensure_meta(quote_mint).await?;
        let fx = self.sol_per_quote(quote_mint).await?;
        let lamports = lamports_from_quote_atoms(quote_atoms, fx, meta.decimals);
        if lamports == 0 {
            bail!("alt-quote FX produced a zero SOL entry");
        }
        Ok(lamports)
    }

    /// Migrated curve: mint → SOL on Jupiter. `out_amount` is quoted lamports.
    pub async fn flatten_mint_to_sol(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
        amount: u64,
    ) -> Result<JupiterBuild> {
        if amount == 0 {
            bail!("jupiter flatten amount is 0");
        }
        let wsol = Pubkey::from_str(WSOL)?;
        let build = self
            .jupiter_build(
                *mint,
                wsol,
                amount,
                *owner,
                JUPITER_FLATTEN_SLIPPAGE_BPS,
                None,
            )
            .await?;
        if build.out_amount == 0 {
            bail!("jupiter mint→SOL out_amount=0");
        }
        Ok(build)
    }

    pub fn combo_instructions(legs: &AltBuyLegs) -> Vec<Instruction> {
        let mut ixs = legs
            .jupiter
            .instructions
            .iter()
            .filter(|ix| !is_compute_budget(ix))
            .cloned()
            .collect::<Vec<_>>();
        ixs.extend(legs.curve.iter().cloned());
        ixs
    }

    /// Quote → SOL instructions for whatever is sitting in the quote ATA.
    pub async fn reclaim_build(
        &self,
        owner: &Pubkey,
        quote_mint: &Pubkey,
    ) -> Result<Option<JupiterBuild>> {
        let balance = self.quote_balance(owner, quote_mint).await?;
        if balance == 0 {
            return Ok(None);
        }
        let wsol = Pubkey::from_str(WSOL)?;
        let routes = [
            JupRoute {
                only_direct: true,
                max_accounts: 24,
                dexes: None,
            },
            JupRoute {
                only_direct: false,
                max_accounts: 64,
                dexes: None,
            },
        ];
        let mut last_err = anyhow!("no quote→SOL route");
        for route in &routes {
            match self
                .jupiter_build(*quote_mint, wsol, balance, *owner, JUPITER_RECLAIM_SLIPPAGE_BPS, Some(route))
                .await
            {
                Ok(parts) if parts.out_amount > 0 => return Ok(Some(parts)),
                Ok(_) => last_err = anyhow!("jupiter quote→SOL out_amount=0"),
                Err(err) => last_err = err,
            }
        }
        Err(last_err)
    }

    async fn jupiter_sol_to_quote(
        &self,
        owner: &Pubkey,
        quote_mint: &Pubkey,
        sol_lamports: u64,
    ) -> Result<JupiterBuild> {
        let wsol = Pubkey::from_str(WSOL)?;
        let routes = [
            JupRoute {
                only_direct: true,
                max_accounts: 20,
                dexes: Some("Whirlpool"),
            },
            JupRoute {
                only_direct: true,
                max_accounts: 24,
                dexes: None,
            },
        ];
        let mut last_err = anyhow!("no direct SOL→{quote_mint} route");
        for route in &routes {
            match self
                .jupiter_build(
                    wsol,
                    *quote_mint,
                    sol_lamports,
                    *owner,
                    JUPITER_BUY_SLIPPAGE_BPS,
                    Some(route),
                )
                .await
            {
                Ok(parts) if parts.out_amount > 0 => return Ok(parts),
                Ok(_) => {
                    last_err = anyhow!(
                        "jupiter direct SOL→{quote_mint} out_amount=0 (dex={:?})",
                        route.dexes
                    );
                }
                Err(err) => last_err = err,
            }
        }
        Err(last_err)
    }

    async fn sol_per_quote(&self, quote_mint: &Pubkey) -> Result<f64> {
        if let Some(fx) = self.fx.lock().expect("fx").get(quote_mint).cloned() {
            if fx.at.elapsed() <= FX_MAX_AGE && fx.sol_per_quote > 0.0 {
                return Ok(fx.sol_per_quote);
            }
        }
        let key = jupiter_api_key();
        let url = format!("{JUPITER_PRICE}?ids={quote_mint},{WSOL}");
        let body: Value = apply_key(self.http.get(&url), key.as_deref())
            .send()
            .await
            .context("jupiter price http")?
            .error_for_status()
            .context("jupiter price status")?
            .json()
            .await
            .context("jupiter price json")?;
        let quote_usd = usd_price(&body, &quote_mint.to_string())
            .ok_or_else(|| anyhow!("no jupiter price for {quote_mint}"))?;
        let sol_usd = usd_price(&body, WSOL).ok_or_else(|| anyhow!("no jupiter SOL price"))?;
        if sol_usd <= 0.0 || quote_usd <= 0.0 {
            bail!("invalid jupiter usd price");
        }
        let sol_per_quote = quote_usd / sol_usd;
        self.fx.lock().expect("fx").insert(
            *quote_mint,
            FxEntry {
                sol_per_quote,
                at: Instant::now(),
            },
        );
        Ok(sol_per_quote)
    }

    async fn ensure_meta(&self, quote_mint: &Pubkey) -> Result<QuoteMeta> {
        if let Some(meta) = self.meta.lock().expect("meta").get(quote_mint).cloned() {
            return Ok(meta);
        }
        let account = self
            .rpc
            .get_account(quote_mint)
            .await
            .with_context(|| format!("quote mint {quote_mint}"))?;
        if account.data.len() < 45 {
            bail!("quote mint {quote_mint} account is too short");
        }
        let meta = QuoteMeta {
            decimals: account.data[44],
            token_program: account.owner,
        };
        self.meta.lock().expect("meta").insert(*quote_mint, meta.clone());
        Ok(meta)
    }

    async fn jupiter_build(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: u64,
        taker: Pubkey,
        slippage_bps: u16,
        route: Option<&JupRoute>,
    ) -> Result<JupiterBuild> {
        let mut url = format!(
            "{JUPITER_BUILD}?inputMint={input_mint}&outputMint={output_mint}&amount={amount}&taker={taker}&slippageBps={slippage_bps}"
        );
        if let Some(route) = route {
            if route.only_direct {
                url.push_str("&onlyDirectRoutes=true");
            }
            url.push_str(&format!("&maxAccounts={}", route.max_accounts));
            if let Some(dexes) = route.dexes {
                url.push_str("&dexes=");
                url.push_str(dexes);
            }
        }
        let key = jupiter_api_key();
        let build: Value = apply_key(self.http.get(&url), key.as_deref())
            .send()
            .await
            .context("jupiter build http")?
            .error_for_status()
            .with_context(|| format!("jupiter build {input_mint}→{output_mint}"))?
            .json()
            .await
            .context("jupiter build json")?;
        let out_amount = json_u64(build.get("outAmount"));
        let other_amount_threshold = json_u64(build.get("otherAmountThreshold"));
        let mut instructions = Vec::new();
        if let Some(arr) = build.get("setupInstructions").and_then(Value::as_array) {
            for item in arr {
                instructions.push(parse_ix(item)?);
            }
        }
        let swap = build
            .get("swapInstruction")
            .ok_or_else(|| anyhow!("jupiter build missing swapInstruction"))?;
        instructions.push(parse_ix(swap)?);
        if let Some(cleanup) = build.get("cleanupInstruction").filter(|v| !v.is_null()) {
            instructions.push(parse_ix(cleanup)?);
        }
        if let Some(arr) = build.get("otherInstructions").and_then(Value::as_array) {
            for item in arr {
                instructions.push(parse_ix(item)?);
            }
        }
        let mut alts = Vec::new();
        if let Some(map) = build
            .get("addressesByLookupTableAddress")
            .and_then(Value::as_object)
        {
            for (key, addresses) in map {
                let Ok(table) = Pubkey::from_str(key) else {
                    continue;
                };
                let Some(arr) = addresses.as_array() else {
                    continue;
                };
                let addresses: Vec<Pubkey> = arr
                    .iter()
                    .filter_map(|a| a.as_str().and_then(|s| Pubkey::from_str(s).ok()))
                    .collect();
                if !addresses.is_empty() {
                    alts.push(AddressLookupTableAccount {
                        key: table,
                        addresses,
                    });
                }
            }
        }
        Ok(JupiterBuild {
            instructions,
            alts,
            out_amount,
            other_amount_threshold,
        })
    }
}

struct JupRoute {
    only_direct: bool,
    max_accounts: u32,
    dexes: Option<&'static str>,
}

pub fn lamports_from_quote_atoms(atoms: u64, sol_per_quote: f64, decimals: u8) -> u64 {
    if atoms == 0 || sol_per_quote <= 0.0 || !sol_per_quote.is_finite() {
        return 0;
    }
    let sol = atoms as f64 / 10f64.powi(decimals as i32) * sol_per_quote;
    if !sol.is_finite() || sol <= 0.0 {
        return 0;
    }
    (sol * 1_000_000_000.0).round() as u64
}

pub fn scale_price(price: f64, sol_per_quote: f64, decimals: u8) -> f64 {
    if price <= 0.0 || sol_per_quote <= 0.0 || !price.is_finite() || !sol_per_quote.is_finite() {
        return 0.0;
    }
    price * sol_per_quote * 1_000_000_000.0 / 10f64.powi(decimals as i32)
}

fn jupiter_api_key() -> Option<String> {
    let configured = crate::bindings::JUPITER_API_KEY.trim();
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    std::env::var("JUPITER_API_KEY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn apply_key(request: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    match api_key {
        Some(key) => request.header("x-api-key", key),
        None => request,
    }
}

fn usd_price(body: &Value, mint: &str) -> Option<f64> {
    body.pointer(&format!("/{mint}/usdPrice"))
        .and_then(json_f64)
        .or_else(|| body.pointer(&format!("/{mint}/price")).and_then(json_f64))
        .or_else(|| {
            body.pointer(&format!("/data/{mint}/price"))
                .and_then(json_f64)
        })
}

fn json_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .filter(|n| n.is_finite() && *n > 0.0)
}

fn json_u64(value: Option<&Value>) -> u64 {
    value
        .and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64()))
        .unwrap_or(0)
}

fn is_compute_budget(ix: &Instruction) -> bool {
    ix.program_id
        == Pubkey::from_str("ComputeBudget111111111111111111111111111111").unwrap()
}

fn parse_ix(value: &Value) -> Result<Instruction> {
    let program_id = Pubkey::from_str(
        value
            .get("programId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("jupiter ix missing programId"))?,
    )?;
    let accounts = value
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("jupiter ix missing accounts"))?;
    let mut metas = Vec::with_capacity(accounts.len());
    for account in accounts {
        let pubkey = Pubkey::from_str(
            account
                .get("pubkey")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("jupiter ix account missing pubkey"))?,
        )?;
        let signer = account
            .get("isSigner")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let writable = account
            .get("isWritable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        metas.push(AccountMeta {
            pubkey,
            is_signer: signer,
            is_writable: writable,
        });
    }
    let data = value
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("jupiter ix missing data"))?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("decode jupiter ix")?;
    Ok(Instruction {
        program_id,
        accounts: metas,
        data,
    })
}

pub fn too_large(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    text.contains("too large") || text.contains("encoded too large")
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;

    #[test]
    fn a_missing_rate_is_not_fresh() {
        let quotes = AltQuote::new(std::sync::Arc::new(
            solana_client::nonblocking::rpc_client::RpcClient::new("http://127.0.0.1:8899".into()),
        ));
        assert!(quotes.fresh_sol_per_quote(&Pubkey::new_unique()).is_none());
    }

    #[test]
    fn scale_from_cache_needs_both_fx_and_meta() {
        let quotes = AltQuote::new(std::sync::Arc::new(
            solana_client::nonblocking::rpc_client::RpcClient::new("http://127.0.0.1:8899".into()),
        ));
        let quote = Pubkey::new_unique();
        let mut atoms = 1_000_000u64;
        let mut price = 1_000.0;
        assert!(!quotes.scale_from_cache(&quote, &mut atoms, &mut price));
        assert_eq!(atoms, 1_000_000);

        quotes.fx.lock().expect("fx").insert(
            quote,
            FxEntry {
                sol_per_quote: 0.005,
                at: Instant::now(),
            },
        );
        // FX alone is not enough — mint meta must already be cached.
        assert!(!quotes.scale_from_cache(&quote, &mut atoms, &mut price));

        quotes.meta.lock().expect("meta").insert(
            quote,
            QuoteMeta {
                decimals: 6,
                token_program: *ids::TOKEN_PROGRAM,
            },
        );
        assert!(quotes.scale_from_cache(&quote, &mut atoms, &mut price));
        assert_eq!(atoms, 5_000_000);
        assert!((price - 5_000.0).abs() < 1e-6);
        assert_eq!(quotes.quote_atoms_to_sol_cached(&quote, 1_000_000), Some(0.005));
    }

    #[test]
    fn usdc_atoms_become_sol_lamports() {
        // 1 USDC at 0.005 SOL each is 0.005 SOL.
        assert_eq!(lamports_from_quote_atoms(1_000_000, 0.005, 6), 5_000_000);
        let price = scale_price(1_000.0, 0.005, 6);
        assert!((price - 5_000.0).abs() < 1e-6);
    }
}
