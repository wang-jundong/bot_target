//! One-transaction route from SOL into any non-SOL pump pair, and back.
//!
//! Pump-coin quotes walk the bonding-curve / PumpSwap chain and trade it with
//! `multi_hop_swap`. A quote that is not a pump coin (USDC, an xStock, any
//! other listed mint) adds a Jupiter SOL leg in that same transaction.

use super::ids::{
    PUMP_AMM_PROGRAM, PUMP_BUYBACK_FEE_RECIPIENTS, PUMP_PROGRAM, TOKEN_PROGRAM, WSOL_MINT,
    amm_event_authority, amm_fee_config, amm_global_config, amm_user_volume, ata,
    bonding_curve_pda, canonical_pool_pda, parse_amm_global_config, parse_amm_pool,
    pump_event_authority, pump_fee_config, pump_global, pump_user_volume,
};
use super::jupiter::{self, Leg};
use super::quote::slippage_down;
use super::trade::{close_account_instruction, create_ata_idempotent, wrap_wsol};
use crate::events::{PumpCurveSnapshot, PumpSwapSnapshot};
use crate::state::PreparedTrade;
use anyhow::{Context, Result, bail};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_system_interface::program as system_program;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const MULTI_HOP: [u8; 8] = [43, 100, 73, 19, 233, 246, 111, 148];
const BUY_EXACT_QUOTE_IN_V3: [u8; 8] = [225, 247, 80, 30, 213, 179, 132, 136];
const SELL_V3: [u8; 8] = [28, 146, 222, 119, 38, 196, 105, 213];
/// Handoff between the pump hop and the SOL leg. Wider user slippage still
/// applies to the final token amount (buys) or the SOL Jupiter will accept (sells).
const LEG_SLIPPAGE_BPS: u64 = 100;
const FRESH: Duration = Duration::from_secs(2);
/// A multi-hop route can be a few seconds behind the background refresh.
/// User slippage covers that move, so the send does not wait on another read.
const RESERVE_TRUST: Duration = Duration::from_secs(10);
const PRICE_FRESH: Duration = Duration::from_secs(10);
const FAIL_FRESH: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Market {
    Curve,
    Pool,
}

#[derive(Clone, Debug)]
struct Hop {
    market: Market,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    base_program: Pubkey,
    quote_program: Pubkey,
    address: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
    base_reserve: u128,
    quote_reserve: u128,
    virtual_quote: i128,
    protocol_fee_bps: u128,
    lp_fee_bps: u128,
    creator_fee_bps: u128,
    mayhem: bool,
    cashback: bool,
}

impl Hop {
    fn effective_quote(&self) -> u128 {
        match self.market {
            Market::Curve => self.quote_reserve,
            Market::Pool => {
                let reserve = self.quote_reserve as i128 + self.virtual_quote;
                if reserve <= 0 { 0 } else { reserve as u128 }
            }
        }
    }
}

#[derive(Clone, Debug)]
enum Terminal {
    Sol,
    External {
        mint: Pubkey,
        program: Pubkey,
        decimals: u8,
    },
}

#[derive(Clone, Debug)]
struct Route {
    hops: Vec<Hop>,
    terminal: Terminal,
    lamports_per_terminal_raw: f64,
    price_at: Instant,
}

impl Route {
    fn settles_in_sol(&self) -> bool {
        matches!(self.terminal, Terminal::Sol)
    }

    fn terminal_mint(&self) -> Pubkey {
        match &self.terminal {
            Terminal::Sol => *WSOL_MINT,
            Terminal::External { mint, .. } => *mint,
        }
    }

    fn terminal_program(&self) -> Pubkey {
        match &self.terminal {
            Terminal::Sol => *TOKEN_PROGRAM,
            Terminal::External { program, .. } => *program,
        }
    }

    fn terminal_name(&self) -> String {
        match &self.terminal {
            Terminal::Sol => "SOL".to_string(),
            Terminal::External { mint, .. } => mint.to_string(),
        }
    }

    /// Lamports per raw unit of the coin the target is priced in.
    fn scale(&self) -> f64 {
        let n = self.hops.len();
        if n == 0 || !self.lamports_per_terminal_raw.is_finite() {
            return 0.0;
        }
        if n == 1 {
            return self.lamports_per_terminal_raw;
        }
        let mut lamports = self.lamports_per_terminal_raw;
        for hop in &self.hops[..n - 1] {
            let base = hop.base_reserve;
            if base == 0 {
                return 0.0;
            }
            lamports *= hop.effective_quote() as f64 / base as f64;
        }
        lamports
    }

    fn quote_buy(&self, quote_in: u128) -> u128 {
        walk(&self.hops, quote_in, true)
    }

    fn quote_sell(&self, token_in: u128) -> u128 {
        let reversed: Vec<Hop> = self.hops.iter().rev().cloned().collect();
        walk(&reversed, token_in, false)
    }
}

#[derive(Clone, Copy)]
struct GlobalFees {
    lp: u128,
    protocol: u128,
    creator: u128,
}

#[derive(Clone)]
struct Entry {
    route: Option<Route>,
    error: Option<String>,
    at: Instant,
}

pub struct Book {
    entries: RwLock<HashMap<String, Entry>>,
    fees: RwLock<Option<GlobalFees>>,
    /// One in-flight load per mint. Other mints are not blocked, and the lock
    /// is not held across the Jupiter price request.
    gates: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Book {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            fees: RwLock::new(None),
            gates: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn gate(&self, key: &str) -> Arc<Mutex<()>> {
        let mut gates = self.gates.lock().expect("route gates");
        gates
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn fresh(&self, rpc: &RpcClient, mint: &Pubkey) -> Result<Route> {
        self.load(rpc, mint, true).await
    }

    /// Scale already known for this mint, or for its quote when that quote
    /// was priced before. The tape uses this and does not wait on RPC.
    pub fn cached_event_scale(&self, mint: &Pubkey, quote: Option<&Pubkey>) -> Option<f64> {
        if let Some((route, _)) = self.cached_entry(mint) {
            let scale = route.scale();
            return (scale > 0.0).then_some(scale);
        }
        quote.and_then(jupiter::tape_rate)
    }

    pub async fn lamports_per_quote(&self, rpc: &RpcClient, mint: &Pubkey) -> Result<f64> {
        let scale = self.load(rpc, mint, false).await?.scale();
        if scale <= 0.0 {
            bail!("no SOL price for quote of {mint}");
        }
        Ok(scale)
    }

    async fn load(&self, rpc: &RpcClient, mint: &Pubkey, force: bool) -> Result<Route> {
        let key = mint.to_string();
        if let Some(route) = self.cached_fresh(&key, force) {
            return Ok(route);
        }
        let gate = self.gate(&key);
        let _hold = gate.lock().await;
        let route = self.load_gated(rpc, mint, force).await?;
        drop(_hold);
        if force {
            return Ok(route);
        }
        self.refresh_terminal_price(&key, route).await
    }

    fn cached_entry(&self, mint: &Pubkey) -> Option<(Route, Duration)> {
        let entries = self.entries.read().expect("routes");
        let entry = entries.get(&mint.to_string())?;
        Some((entry.route.clone()?, entry.at.elapsed()))
    }

    fn cached_fresh(&self, key: &str, force: bool) -> Option<Route> {
        if force {
            return None;
        }
        let entries = self.entries.read().expect("routes");
        let entry = entries.get(key)?;
        let route = entry.route.clone()?;
        if entry.at.elapsed() < FRESH {
            Some(route)
        } else {
            None
        }
    }

    async fn refresh_terminal_price(&self, key: &str, mut route: Route) -> Result<Route> {
        let (mint, decimals) = match &route.terminal {
            Terminal::External { mint, decimals, .. } => (*mint, *decimals),
            Terminal::Sol => return Ok(route),
        };
        if route.price_at.elapsed() <= PRICE_FRESH {
            return Ok(route);
        }
        match jupiter::lamports_per_raw(&mint, decimals).await {
            Ok(rate) => {
                route.lamports_per_terminal_raw = rate;
                route.price_at = Instant::now();
                self.save(key, &route);
                Ok(route)
            }
            Err(err) => {
                tracing::info!(
                    error = %err,
                    mint = %key,
                    "[ROUTE] Keeping the previous SOL price"
                );
                Ok(route)
            }
        }
    }

    async fn load_gated(&self, rpc: &RpcClient, mint: &Pubkey, force: bool) -> Result<Route> {
        let key = mint.to_string();
        let cached = self.entries.read().expect("routes").get(&key).cloned();
        if let Some(entry) = cached {
            if entry.route.is_none()
                && !force
                && entry.at.elapsed() < FAIL_FRESH
                && let Some(error) = entry.error
            {
                bail!(error);
            }
            if let Some(route) = entry.route.clone() {
                if !force && entry.at.elapsed() < FRESH {
                    return Ok(route);
                }
                match refresh(rpc, &route).await {
                    Ok(updated) => {
                        self.save(&key, &updated);
                        return Ok(updated);
                    }
                    Err(err) if !force && !err.to_string().contains("rediscover") => {
                        return Ok(route);
                    }
                    Err(err) => {
                        tracing::info!(
                            error = %err,
                            mint = %key,
                            "[ROUTE] Resolving non-SOL pair again"
                        );
                    }
                }
            }
        }
        match discover(rpc, self, *mint).await {
            Ok(route) => {
                self.save(&key, &route);
                Ok(route)
            }
            Err(err) => {
                self.save_err(&key, &err.to_string());
                Err(err)
            }
        }
    }

    fn save(&self, key: &str, route: &Route) {
        self.entries.write().expect("routes").insert(
            key.to_string(),
            Entry {
                route: Some(route.clone()),
                error: None,
                at: Instant::now(),
            },
        );
    }

    fn save_err(&self, key: &str, error: &str) {
        self.entries.write().expect("routes").insert(
            key.to_string(),
            Entry {
                route: None,
                error: Some(error.to_string()),
                at: Instant::now(),
            },
        );
    }

    async fn global_fees(&self, rpc: &RpcClient) -> Result<GlobalFees> {
        if let Some(fees) = *self.fees.read().expect("fees") {
            return Ok(fees);
        }
        let account = one(rpc, &amm_global_config())
            .await?
            .context("missing pumpswap global config")?;
        let parsed = parse_amm_global_config(&account.data).context("pumpswap global config")?;
        let fees = GlobalFees {
            lp: parsed.lp_fee_bps as u128,
            protocol: parsed.protocol_fee_bps as u128,
            creator: parsed.coin_creator_fee_bps as u128,
        };
        *self.fees.write().expect("fees") = Some(fees);
        Ok(fees)
    }
}

async fn execute(
    owner: &Pubkey,
    mint: &Pubkey,
    route: &Route,
    buy: bool,
    amount: u64,
    slippage_bps: u64,
    compute_unit_limit: u32,
    priority_fee_lamports: u64,
    tip_lamports: u64,
) -> Result<PreparedTrade> {
    if amount == 0 || route.hops.is_empty() {
        bail!("empty non-SOL route");
    }
    let (pump_in, min_out, leg) = if buy {
        if route.settles_in_sol() {
            let min_out = as_u64(slippage_down(route.quote_buy(amount as u128), slippage_bps));
            if min_out == 0 {
                bail!("buy quotes zero tokens");
            }
            (amount, min_out, None)
        } else {
            let leg = jupiter::swap(
                owner,
                &WSOL_MINT,
                &route.terminal_mint(),
                amount,
                LEG_SLIPPAGE_BPS,
            )
            .await?;
            let quoted = route.quote_buy(leg.min_out as u128);
            note_expected_buy(*owner, *mint, as_u64(quoted));
            let min_out = as_u64(slippage_down(quoted, slippage_bps));
            if leg.min_out == 0 || min_out == 0 {
                bail!("buy quotes zero tokens");
            }
            (leg.min_out, min_out, Some(leg))
        }
    } else if route.settles_in_sol() {
        let min_out = as_u64(slippage_down(
            route.quote_sell(amount as u128),
            slippage_bps,
        ));
        if min_out == 0 {
            bail!("sell quotes zero SOL");
        }
        (amount, min_out, None)
    } else if let Some((aim, leg)) = jupiter::take_sell(owner, &route.terminal_mint(), amount) {
        (amount, aim, Some(leg))
    } else {
        let aim = as_u64(slippage_down(
            route.quote_sell(amount as u128),
            LEG_SLIPPAGE_BPS,
        ));
        if aim == 0 {
            bail!("sell quotes zero quote tokens");
        }
        let leg = jupiter::swap(
            owner,
            &route.terminal_mint(),
            &WSOL_MINT,
            aim,
            slippage_bps.max(LEG_SLIPPAGE_BPS),
        )
        .await?;
        (amount, aim, Some(leg))
    };
    tracing::info!(
        hops = route.hops.len(),
        terminal = %route.terminal_name(),
        buy,
        "[ROUTE] Non-SOL pair buy/sell in one transaction"
    );
    assemble(
        owner,
        route,
        buy,
        pump_in,
        min_out,
        leg,
        compute_unit_limit,
        priority_fee_lamports,
        tip_lamports,
    )
}

const WARM_GAP: Duration = Duration::from_millis(400);

/// True when this wallet and mint should start another warm pass.
pub fn claim_warm(owner: &Pubkey, mint: &str) -> bool {
    static WARMED: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Instant>>> =
        std::sync::OnceLock::new();
    let warmed = WARMED.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut warmed = warmed.lock().expect("warm");
    let key = format!("{owner}:{mint}");
    if let Some(at) = warmed.get(&key)
        && at.elapsed() < WARM_GAP
    {
        return false;
    }
    warmed.insert(key, Instant::now());
    true
}

/// Refresh the Jupiter SOL leg in the background for the strategy's buy size,
/// and for the open token amount on a sell.
pub async fn warm(
    book: &Book,
    rpc: Arc<RpcClient>,
    owner: Pubkey,
    mint: &str,
    buy_lamports: u64,
    sell_tokens: u64,
    sell_slippage: u64,
) {
    if buy_lamports == 0 {
        return;
    }
    let Ok(mint_key) = mint.parse::<Pubkey>() else {
        return;
    };
    let Ok(route) = book.load(&rpc, &mint_key, false).await else {
        return;
    };
    if route.settles_in_sol() {
        return;
    }
    let terminal = route.terminal_mint();
    jupiter::touch(owner, *WSOL_MINT, terminal, buy_lamports, LEG_SLIPPAGE_BPS);
    warm_sell(book, rpc, owner, &mint_key, sell_tokens, sell_slippage).await;
}

/// Start the sell leg while a buy is still confirming, using the token amount
/// the buy quote expects to receive.
pub async fn warm_sell(
    book: &Book,
    rpc: Arc<RpcClient>,
    owner: Pubkey,
    mint: &Pubkey,
    sell_tokens: u64,
    sell_slippage: u64,
) {
    if sell_tokens == 0 {
        return;
    }
    let Ok(route) = book.load(&rpc, mint, false).await else {
        return;
    };
    if route.settles_in_sol() {
        return;
    }
    let aim = as_u64(slippage_down(
        route.quote_sell(sell_tokens as u128),
        LEG_SLIPPAGE_BPS,
    ));
    if aim == 0 {
        return;
    }
    let slippage = sell_slippage.max(LEG_SLIPPAGE_BPS);
    let terminal = route.terminal_mint();
    jupiter::remember_sell(owner, terminal, sell_tokens, aim, slippage);
    jupiter::touch(owner, terminal, *WSOL_MINT, aim, slippage);
}

fn expected_buys() -> &'static std::sync::Mutex<HashMap<(Pubkey, Pubkey), u64>> {
    static BUYS: std::sync::OnceLock<std::sync::Mutex<HashMap<(Pubkey, Pubkey), u64>>> =
        std::sync::OnceLock::new();
    BUYS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn note_expected_buy(owner: Pubkey, mint: Pubkey, tokens: u64) {
    if tokens == 0 {
        return;
    }
    expected_buys()
        .lock()
        .expect("expected buys")
        .insert((owner, mint), tokens);
}

pub fn expected_buy(owner: &Pubkey, mint: &Pubkey) -> Option<u64> {
    expected_buys()
        .lock()
        .expect("expected buys")
        .get(&(*owner, *mint))
        .copied()
        .filter(|tokens| *tokens > 0)
}

pub async fn prepare(
    book: &Book,
    rpc: &RpcClient,
    owner: &Pubkey,
    mint: &Pubkey,
    buy: bool,
    amount: u64,
    slippage_bps: u64,
    compute_unit_limit: u32,
    priority_fee_lamports: u64,
    tip_lamports: u64,
    refresh_route: bool,
    curve: Option<&PumpCurveSnapshot>,
    swap: Option<&PumpSwapSnapshot>,
) -> Result<PreparedTrade> {
    let route = if refresh_route {
        None
    } else if let Some((route, age)) = book.cached_entry(mint) {
        route_ready(&route, age, curve, swap).then_some(route)
    } else {
        None
    };
    let mut route = if let Some(route) = route {
        tracing::info!(hops = route.hops.len(), "[ROUTE] Using cached reserves");
        route
    } else {
        book.fresh(rpc, mint).await?
    };
    overlay(&mut route, curve, swap);
    execute(
        owner,
        mint,
        &route,
        buy,
        amount,
        slippage_bps,
        compute_unit_limit,
        priority_fee_lamports,
        tip_lamports,
    )
    .await
}

/// A single hop is covered by the streamed curve or pool. Extra hops stay
/// usable for several seconds after the background refresh.
fn route_ready(
    route: &Route,
    age: Duration,
    curve: Option<&PumpCurveSnapshot>,
    swap: Option<&PumpSwapSnapshot>,
) -> bool {
    if route.hops.is_empty() || age >= Duration::from_secs(30) {
        return false;
    }
    if route.hops.len() == 1 {
        let hop = &route.hops[0];
        return match hop.market {
            Market::Curve => curve.is_some(),
            Market::Pool => swap.is_some_and(|swap| swap.pool == hop.address.to_string()),
        };
    }
    age < RESERVE_TRUST
}

fn overlay(route: &mut Route, curve: Option<&PumpCurveSnapshot>, swap: Option<&PumpSwapSnapshot>) {
    let Some(hop) = route.hops.last_mut() else {
        return;
    };
    if hop.market == Market::Curve
        && let Some(curve) = curve
    {
        hop.base_reserve = curve.virtual_token_reserves;
        hop.quote_reserve = curve.virtual_quote_reserves;
        hop.protocol_fee_bps = curve.protocol_fee_bps;
        hop.creator_fee_bps = curve.creator_fee_bps;
        hop.mayhem = curve.mayhem_mode;
        hop.cashback = curve.cashback;
    }
    if hop.market == Market::Pool
        && let Some(swap) = swap
        && hop.address.to_string() == swap.pool
    {
        hop.base_reserve = swap.base_reserve;
        hop.quote_reserve = swap.quote_reserve;
        hop.virtual_quote = swap.virtual_quote_reserves;
        hop.lp_fee_bps = swap.lp_fee_bps;
        hop.protocol_fee_bps = swap.protocol_fee_bps;
        hop.creator_fee_bps = swap.coin_creator_fee_bps;
        hop.cashback = swap.cashback;
    }
}

fn assemble(
    owner: &Pubkey,
    route: &Route,
    buy: bool,
    pump_in: u64,
    min_out: u64,
    leg: Option<Leg>,
    compute_unit_limit: u32,
    priority_fee_lamports: u64,
    tip_lamports: u64,
) -> Result<PreparedTrade> {
    if pump_in == 0 || min_out == 0 {
        bail!("route amount is zero");
    }
    let currency = &route.hops[0];
    let target = route.hops.last().expect("hop");
    let terminal_ata = ata(owner, &route.terminal_mint(), &route.terminal_program());
    let base_ata = ata(owner, &target.base_mint, &target.base_program);
    let (buyback_owner, buyback_ata) =
        buyback_account(&currency.quote_mint, &currency.quote_program);
    let (user_in, user_out) = if buy {
        (terminal_ata, base_ata)
    } else {
        (base_ata, terminal_ata)
    };
    let mut instructions = vec![
        create_ata_idempotent(
            owner,
            owner,
            &route.terminal_mint(),
            &route.terminal_program(),
        ),
        create_ata_idempotent(owner, owner, &target.base_mint, &target.base_program),
        create_ata_idempotent(
            owner,
            &buyback_owner,
            &currency.quote_mint,
            &currency.quote_program,
        ),
    ];
    if buy && route.settles_in_sol() && currency.market == Market::Pool {
        instructions.extend(wrap_wsol(
            owner,
            &WSOL_MINT,
            &TOKEN_PROGRAM,
            &terminal_ata,
            pump_in,
        ));
    }
    if buy && let Some(leg) = &leg {
        instructions.extend(leg.instructions.iter().cloned());
    }
    if use_v3(route) {
        instructions.push(v3_ix(
            owner,
            target,
            &user_in,
            &user_out,
            &buyback_ata,
            buy,
            pump_in,
            min_out,
        ));
    } else if multi_hop_allowed(route) {
        instructions.push(multi_hop_ix(
            owner,
            &user_in,
            &user_out,
            &buyback_ata,
            route,
            buy,
            pump_in,
            min_out,
        ));
    } else {
        bail!(
            "non-SOL route ends at {} and includes a mayhem or cashback hop that cannot be traded in one transaction",
            route.terminal_name()
        );
    }
    if !buy && let Some(leg) = &leg {
        instructions.extend(leg.instructions.iter().cloned());
    }
    if !buy && route.settles_in_sol() && currency.market == Market::Pool {
        instructions.push(close_account_instruction(
            &terminal_ata,
            owner,
            owner,
            &TOKEN_PROGRAM,
        ));
    }
    let mut limit = 220_000 + 90_000 * route.hops.len() as u32;
    if leg.is_some() {
        limit = limit.max(1_400_000);
    }
    Ok(PreparedTrade {
        instructions,
        payer: *owner,
        compute_unit_limit: limit.max(compute_unit_limit),
        priority_fee_lamports,
        tip_lamports,
        lookup_tables: leg.map(|item| item.lookup_tables).unwrap_or_default(),
    })
}

fn use_v3(route: &Route) -> bool {
    !multi_hop_allowed(route)
        && route.hops.len() == 1
        && route.hops[0].market == Market::Curve
        && !route.hops[0].cashback
}

fn multi_hop_allowed(route: &Route) -> bool {
    if route.hops.iter().any(|hop| hop.mayhem) {
        return false;
    }
    route
        .hops
        .iter()
        .enumerate()
        .all(|(index, hop)| !hop.cashback || index == 0)
}

fn multi_hop_ix(
    owner: &Pubkey,
    user_in: &Pubkey,
    user_out: &Pubkey,
    buyback_ata: &Pubkey,
    route: &Route,
    buy: bool,
    amount_in: u64,
    min_out: u64,
) -> Instruction {
    let mut accounts = vec![
        meta(*owner, true, true),
        meta(*user_in, true, false),
        meta(*user_out, true, false),
        meta(amm_global_config(), false, false),
        meta(amm_fee_config(), false, false),
        meta(amm_user_volume(owner), true, false),
        meta(*buyback_ata, true, false),
        meta(*TOKEN_PROGRAM, false, false),
        meta(*super::ids::TOKEN_2022_PROGRAM, false, false),
        meta(system_program::id(), false, false),
        meta(amm_event_authority(), false, false),
        meta(*PUMP_AMM_PROGRAM, false, false),
        meta(*PUMP_PROGRAM, false, false),
        meta(pump_global(), false, false),
        meta(pump_fee_config(), false, false),
        meta(pump_event_authority(), false, false),
    ];
    let mut hops: Vec<&Hop> = route.hops.iter().collect();
    if !buy {
        hops.reverse();
    }
    for hop in hops {
        accounts.push(meta(hop.base_mint, false, false));
        accounts.push(meta(hop.quote_mint, false, false));
        accounts.push(meta(hop.address, true, false));
        accounts.push(meta(hop.base_vault, true, false));
        accounts.push(meta(hop.quote_vault, true, false));
    }
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&MULTI_HOP);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    Instruction {
        program_id: *PUMP_AMM_PROGRAM,
        accounts,
        data,
    }
}

fn v3_ix(
    owner: &Pubkey,
    hop: &Hop,
    user_quote: &Pubkey,
    user_base: &Pubkey,
    buyback_ata: &Pubkey,
    buy: bool,
    amount_in: u64,
    min_out: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(25);
    if buy {
        data.extend_from_slice(&BUY_EXACT_QUOTE_IN_V3);
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
        if hop.mayhem {
            data.push(1);
        }
    } else {
        data.extend_from_slice(&SELL_V3);
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
    }
    Instruction {
        program_id: *PUMP_PROGRAM,
        accounts: vec![
            meta(pump_global(), false, false),
            meta(hop.base_mint, false, false),
            meta(hop.quote_mint, false, false),
            meta(hop.base_program, false, false),
            meta(hop.quote_program, false, false),
            meta(hop.address, true, false),
            meta(hop.base_vault, true, false),
            meta(hop.quote_vault, true, false),
            meta(*owner, true, true),
            meta(*user_base, true, false),
            meta(*user_quote, true, false),
            meta(pump_user_volume(owner), true, false),
            meta(pump_fee_config(), false, false),
            meta(*buyback_ata, true, false),
            meta(system_program::id(), false, false),
            meta(pump_event_authority(), false, false),
            meta(*PUMP_PROGRAM, false, false),
        ],
        data,
    }
}

fn buyback_account(quote: &Pubkey, program: &Pubkey) -> (Pubkey, Pubkey) {
    let wallet =
        PUMP_BUYBACK_FEE_RECIPIENTS[rand::random::<usize>() % PUMP_BUYBACK_FEE_RECIPIENTS.len()];
    (wallet, ata(&wallet, quote, program))
}

fn walk(hops: &[Hop], amount_in: u128, buying: bool) -> u128 {
    let mut amount = amount_in;
    for (index, hop) in hops.iter().enumerate() {
        let fee = fee_bps(hops, index, buying);
        amount = if buying {
            buy_base(amount, hop.base_reserve, hop.effective_quote(), fee)
        } else {
            sell_quote(amount, hop.base_reserve, hop.effective_quote(), fee)
        };
    }
    amount
}

fn fee_bps(hops: &[Hop], index: usize, buying: bool) -> u128 {
    let hop = &hops[index];
    let n = hops.len();
    if n == 1 {
        return hop.protocol_fee_bps + hop.lp_fee_bps + hop.creator_fee_bps;
    }
    let currency = if buying { index == 0 } else { index + 1 == n };
    let far = if buying { index + 1 == n } else { index == 0 };
    let mut fee = 0;
    if currency {
        fee += hop.protocol_fee_bps;
    }
    if far {
        fee += hop.lp_fee_bps + hop.creator_fee_bps;
    }
    fee
}

fn buy_base(quote_in: u128, base_reserve: u128, quote_reserve: u128, fee_bps: u128) -> u128 {
    if quote_in == 0 || base_reserve == 0 || quote_reserve == 0 {
        return 0;
    }
    let net = quote_in.saturating_mul(10_000) / (10_000 + fee_bps);
    let net = net.saturating_sub(1);
    if net == 0 {
        return 0;
    }
    net.saturating_mul(base_reserve) / quote_reserve.saturating_add(net)
}

fn sell_quote(base_in: u128, base_reserve: u128, quote_reserve: u128, fee_bps: u128) -> u128 {
    if base_in == 0 || base_reserve == 0 || quote_reserve == 0 {
        return 0;
    }
    let gross = quote_reserve.saturating_mul(base_in) / base_reserve.saturating_add(base_in);
    let kept = 10_000u128.saturating_sub(fee_bps.min(9_999));
    gross.saturating_mul(kept) / 10_000
}

fn as_u64(value: u128) -> u64 {
    value.min(u64::MAX as u128) as u64
}

fn meta(pubkey: Pubkey, writable: bool, signer: bool) -> AccountMeta {
    AccountMeta {
        pubkey,
        is_signer: signer,
        is_writable: writable,
    }
}

async fn discover(rpc: &RpcClient, book: &Book, target: Pubkey) -> Result<Route> {
    let mut hops = Vec::new();
    let mut cursor = target;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(cursor) {
            bail!("quote cycle at {cursor}");
        }
        if hops.len() == 4 {
            bail!("quote chain for {target} is deeper than 4");
        }
        let curve_pda = bonding_curve_pda(&cursor);
        let curve_account = one(rpc, &curve_pda)
            .await?
            .with_context(|| format!("{cursor} has no pump bonding curve"))?;
        let curve = parse_curve(&curve_account.data)
            .with_context(|| format!("bonding curve layout {cursor}"))?;
        let base_program = mint_program(rpc, &cursor).await?;
        let quote_is_sol = is_sol_mint(&curve.quote_mint);
        let quote_mint = trading_mint(curve.quote_mint);
        let quote_program = if quote_is_sol {
            *TOKEN_PROGRAM
        } else {
            mint_program(rpc, &quote_mint).await?
        };
        let hop = if !curve.complete {
            Hop {
                market: Market::Curve,
                base_mint: cursor,
                quote_mint,
                base_program,
                quote_program,
                address: curve_pda,
                base_vault: ata(&curve_pda, &cursor, &base_program),
                quote_vault: ata(&curve_pda, &quote_mint, &quote_program),
                base_reserve: curve.virtual_token as u128,
                quote_reserve: curve.virtual_quote as u128,
                virtual_quote: 0,
                protocol_fee_bps: 100,
                lp_fee_bps: 0,
                creator_fee_bps: if curve.creator == Pubkey::default() {
                    0
                } else {
                    30
                },
                mayhem: curve.mayhem,
                cashback: curve.cashback,
            }
        } else {
            let pool = canonical_pool_pda(&cursor, &quote_mint);
            let pool_account = one(rpc, &pool).await?.with_context(|| {
                format!("{cursor} curve is complete but its pool is not on chain yet")
            })?;
            let parsed = parse_amm_pool(&pool_account.data)
                .with_context(|| format!("pool layout {pool}"))?;
            let fees = book.global_fees(rpc).await?;
            let pool_quote = trading_mint(parsed.quote_mint);
            let pool_quote_program = if is_sol_mint(&parsed.quote_mint) {
                *TOKEN_PROGRAM
            } else {
                mint_program(rpc, &pool_quote).await?
            };
            let base_mint_program = mint_program(rpc, &parsed.base_mint).await?;
            Hop {
                market: Market::Pool,
                base_mint: parsed.base_mint,
                quote_mint: pool_quote,
                base_program: base_mint_program,
                quote_program: pool_quote_program,
                address: pool,
                base_vault: parsed.pool_base_token_account,
                quote_vault: parsed.pool_quote_token_account,
                base_reserve: vault_amount(rpc, &parsed.pool_base_token_account).await?,
                quote_reserve: vault_amount(rpc, &parsed.pool_quote_token_account).await?,
                virtual_quote: parsed.virtual_quote_reserves,
                protocol_fee_bps: fees.protocol,
                lp_fee_bps: fees.lp,
                creator_fee_bps: if parsed.coin_creator == Pubkey::default() {
                    0
                } else {
                    fees.creator
                },
                mayhem: pool_account.data.get(8 + 235).copied().unwrap_or(0) != 0,
                cashback: parsed.cashback,
            }
        };
        let next = hop.quote_mint;
        let next_is_sol = is_sol_mint(&next);
        hops.push(hop);
        if next_is_sol {
            return Ok(finish(hops, Terminal::Sol, 1.0));
        }
        // An empty or non-pump account at the bonding-curve address is not a
        // hop. PUMP's own curve address is a system account with no data.
        let next_curve = one(rpc, &bonding_curve_pda(&next)).await?;
        if !next_curve
            .as_ref()
            .is_some_and(|account| is_bonding_curve(&account.owner, &account.data))
        {
            let (program, decimals) = mint_meta(rpc, &next).await?;
            let rate = jupiter::lamports_per_raw(&next, decimals).await?;
            return Ok(finish(
                hops,
                Terminal::External {
                    mint: next,
                    program,
                    decimals,
                },
                rate,
            ));
        }
        cursor = next;
    }
}

async fn refresh(rpc: &RpcClient, route: &Route) -> Result<Route> {
    let mut keys = Vec::new();
    for hop in &route.hops {
        keys.push(hop.address);
        if hop.market == Market::Pool {
            keys.push(hop.base_vault);
            keys.push(hop.quote_vault);
        }
    }
    let accounts = rpc
        .get_multiple_accounts(&keys)
        .await
        .context("refresh route reserves")?;
    let mut route = route.clone();
    let mut index = 0;
    for hop in &mut route.hops {
        let account = accounts
            .get(index)
            .and_then(|item| item.as_ref())
            .with_context(|| format!("missing market {}", hop.address))?;
        index += 1;
        match hop.market {
            Market::Curve => {
                let curve = parse_curve(&account.data).context("curve refresh")?;
                if curve.complete {
                    bail!("rediscover: {} migrated", hop.base_mint);
                }
                hop.base_reserve = curve.virtual_token as u128;
                hop.quote_reserve = curve.virtual_quote as u128;
            }
            Market::Pool => {
                if let Some(parsed) = parse_amm_pool(&account.data) {
                    hop.virtual_quote = parsed.virtual_quote_reserves;
                }
                let base = accounts.get(index).and_then(|item| item.as_ref());
                let quote = accounts.get(index + 1).and_then(|item| item.as_ref());
                index += 2;
                if let Some(base) = base {
                    hop.base_reserve = token_amount(&base.data) as u128;
                }
                if let Some(quote) = quote {
                    hop.quote_reserve = token_amount(&quote.data) as u128;
                }
            }
        }
    }
    Ok(route)
}

struct CurveRaw {
    virtual_token: u64,
    virtual_quote: u64,
    complete: bool,
    creator: Pubkey,
    mayhem: bool,
    cashback: bool,
    quote_mint: Pubkey,
}

/// The walk records the target first and the SOL-side market last. Swaps,
/// fees, and the SOL price all start from the SOL side.
fn finish(mut hops: Vec<Hop>, terminal: Terminal, rate: f64) -> Route {
    hops.reverse();
    Route {
        hops,
        terminal,
        lamports_per_terminal_raw: rate,
        price_at: Instant::now(),
    }
}

fn is_bonding_curve(owner: &Pubkey, data: &[u8]) -> bool {
    *owner == *PUMP_PROGRAM && parse_curve(data).is_some()
}

fn parse_curve(data: &[u8]) -> Option<CurveRaw> {
    const CORE: usize = 8 + 5 * 8 + 1 + 32;
    if data.len() < CORE {
        return None;
    }
    let body = &data[8..];
    let virtual_token = u64::from_le_bytes(body[0..8].try_into().ok()?);
    let virtual_quote = u64::from_le_bytes(body[8..16].try_into().ok()?);
    let complete = body[40] != 0;
    let creator = Pubkey::new_from_array(body[41..73].try_into().ok()?);
    let mayhem = body.get(73).copied().unwrap_or(0) != 0;
    let cashback = body.get(74).copied().unwrap_or(0) != 0;
    let quote_mint = if body.len() >= 75 + 32 {
        Pubkey::new_from_array(body[75..107].try_into().ok()?)
    } else {
        Pubkey::default()
    };
    Some(CurveRaw {
        virtual_token,
        virtual_quote,
        complete,
        creator,
        mayhem,
        cashback,
        quote_mint,
    })
}

fn is_sol_mint(mint: &Pubkey) -> bool {
    *mint == Pubkey::default() || *mint == *WSOL_MINT || *mint == system_program::id()
}

fn trading_mint(mint: Pubkey) -> Pubkey {
    if is_sol_mint(&mint) { *WSOL_MINT } else { mint }
}

fn token_amount(data: &[u8]) -> u64 {
    if data.len() < 72 {
        return 0;
    }
    u64::from_le_bytes(data[64..72].try_into().unwrap_or([0; 8]))
}

async fn one(rpc: &RpcClient, key: &Pubkey) -> Result<Option<solana_sdk::account::Account>> {
    let mut got = rpc
        .get_multiple_accounts(&[*key])
        .await
        .with_context(|| format!("account {key}"))?;
    Ok(got.pop().flatten())
}

async fn mint_program(rpc: &RpcClient, mint: &Pubkey) -> Result<Pubkey> {
    Ok(mint_meta(rpc, mint).await?.0)
}

async fn mint_meta(rpc: &RpcClient, mint: &Pubkey) -> Result<(Pubkey, u8)> {
    let account = one(rpc, mint)
        .await?
        .with_context(|| format!("mint {mint} is missing"))?;
    if account.owner != *TOKEN_PROGRAM && account.owner != *super::ids::TOKEN_2022_PROGRAM {
        bail!("mint {mint} is not an SPL token");
    }
    let decimals = account.data.get(44).copied().unwrap_or(0);
    Ok((account.owner, decimals))
}

async fn vault_amount(rpc: &RpcClient, vault: &Pubkey) -> Result<u128> {
    let Some(account) = one(rpc, vault).await? else {
        return Ok(0);
    };
    Ok(token_amount(&account.data) as u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(market: Market, base: u128, quote: u128, mayhem: bool, cashback: bool) -> Hop {
        Hop {
            market,
            base_mint: Pubkey::new_unique(),
            quote_mint: Pubkey::new_unique(),
            base_program: *TOKEN_PROGRAM,
            quote_program: *TOKEN_PROGRAM,
            address: Pubkey::new_unique(),
            base_vault: Pubkey::new_unique(),
            quote_vault: Pubkey::new_unique(),
            base_reserve: base,
            quote_reserve: quote,
            virtual_quote: 0,
            protocol_fee_bps: 100,
            lp_fee_bps: 20,
            creator_fee_bps: 30,
            mayhem,
            cashback,
        }
    }

    fn sample(hops: Vec<Hop>, sol: bool) -> Route {
        Route {
            hops,
            terminal: if sol {
                Terminal::Sol
            } else {
                Terminal::External {
                    mint: Pubkey::new_unique(),
                    program: *TOKEN_PROGRAM,
                    decimals: 6,
                }
            },
            lamports_per_terminal_raw: if sol { 1.0 } else { 5.0 },
            price_at: Instant::now(),
        }
    }

    #[test]
    fn a_quote_chain_is_stored_sol_side_first() {
        let target = hop(Market::Curve, 4_000, 8_000, false, false);
        let target_mint = target.base_mint;
        let mut currency = hop(Market::Pool, 2_000, 10_000, false, false);
        currency.quote_mint = *WSOL_MINT;
        let route = finish(vec![target, currency], Terminal::Sol, 1.0);
        assert_eq!(route.hops[0].quote_mint, *WSOL_MINT);
        assert_eq!(route.hops.last().unwrap().base_mint, target_mint);
        assert_eq!(route.scale(), 5.0);
    }

    #[test]
    fn an_empty_account_is_not_a_bonding_curve() {
        assert!(!is_bonding_curve(&system_program::id(), &[]));
        assert!(!is_bonding_curve(&*PUMP_PROGRAM, &[]));
        assert!(is_bonding_curve(&*PUMP_PROGRAM, &[0; 81]));
    }

    #[test]
    fn a_known_quote_scales_without_loading_the_route() {
        let book = Book::new();
        let quote = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        jupiter::remember_rate(&quote, 2.5);
        assert_eq!(book.cached_event_scale(&mint, Some(&quote)), Some(2.5));
        let mut direct = sample(vec![hop(Market::Curve, 1, 1, false, false)], false);
        direct.lamports_per_terminal_raw = 9.0;
        book.save(&mint.to_string(), &direct);
        assert_eq!(book.cached_event_scale(&mint, Some(&quote)), Some(9.0));
    }

    #[test]
    fn scale_prices_the_targets_quote_in_lamports() {
        let chained = sample(
            vec![
                hop(Market::Pool, 2_000, 10_000, false, false),
                hop(Market::Curve, 1, 1, false, false),
            ],
            true,
        );
        assert_eq!(chained.scale(), 5.0);
        let direct = sample(vec![hop(Market::Curve, 1, 1, false, false)], false);
        assert_eq!(direct.scale(), 5.0);
    }

    #[test]
    fn streamed_single_hop_skips_a_reserve_read() {
        let curve = PumpCurveSnapshot {
            virtual_quote_reserves: 1,
            virtual_token_reserves: 1,
            real_token_reserves: 1,
            creator: String::new(),
            mayhem_mode: false,
            quote_mint: None,
            protocol_fee_bps: 100,
            creator_fee_bps: 0,
            fee_recipient: None,
            cashback: false,
        };
        let one = sample(vec![hop(Market::Curve, 1, 1, false, false)], false);
        assert!(route_ready(
            &one,
            Duration::from_secs(20),
            Some(&curve),
            None
        ));
        assert!(!route_ready(&one, Duration::from_secs(20), None, None));
        assert!(!route_ready(
            &one,
            Duration::from_secs(30),
            Some(&curve),
            None
        ));
        let two = sample(
            vec![
                hop(Market::Pool, 1, 1, false, false),
                hop(Market::Curve, 1, 1, false, false),
            ],
            true,
        );
        assert!(route_ready(&two, Duration::from_secs(9), None, None));
        assert!(!route_ready(&two, Duration::from_secs(10), None, None));
    }

    #[test]
    fn middle_hop_is_free_and_ends_pay_once() {
        let hops = vec![
            hop(Market::Pool, 1, 1, false, false),
            hop(Market::Curve, 1, 1, false, false),
            hop(Market::Curve, 1, 1, false, false),
        ];
        assert_eq!(fee_bps(&hops, 0, true), 100);
        assert_eq!(fee_bps(&hops, 1, true), 0);
        assert_eq!(fee_bps(&hops, 2, true), 50);
        assert_eq!(fee_bps(&hops, 0, false), 50);
        assert_eq!(fee_bps(&hops, 2, false), 100);
        assert_eq!(fee_bps(&hops[..1], 0, true), 150);
    }

    #[test]
    fn buy_then_sell_round_trips_inside_the_pool() {
        let hops = vec![hop(Market::Curve, 1_000_000, 1_000_000, false, false)];
        let out = walk(&hops, 1_000, true);
        assert!(out > 900 && out < 1_000);
        let back = walk(&hops, out, false);
        assert!(back < 1_000);
    }

    #[test]
    fn sol_pool_buy_wraps_and_sell_unwraps_in_one_instruction_list() {
        let owner = Pubkey::new_unique();
        let mut first = hop(Market::Pool, 1_000_000, 2_000_000, false, false);
        first.quote_mint = *WSOL_MINT;
        let route = sample(
            vec![
                first,
                hop(Market::Curve, 5_000_000, 5_000_000, false, false),
            ],
            true,
        );
        let buy = assemble(&owner, &route, true, 1_000_000, 10, None, 100_000, 1, 1).unwrap();
        let swap = buy
            .instructions
            .iter()
            .find(|ix| ix.program_id == *PUMP_AMM_PROGRAM)
            .unwrap();
        assert_eq!(&swap.data[..8], &MULTI_HOP);
        assert_eq!(swap.accounts.len(), 16 + 10);
        assert!(
            buy.instructions
                .iter()
                .any(|ix| ix.program_id == system_program::id())
        );
        let sell = assemble(&owner, &route, false, 1_000, 10, None, 100_000, 1, 1).unwrap();
        assert_eq!(sell.instructions.last().unwrap().data, vec![9]);
        let sell_swap = sell
            .instructions
            .iter()
            .find(|ix| ix.program_id == *PUMP_AMM_PROGRAM)
            .unwrap();
        assert_eq!(sell_swap.accounts[16].pubkey, route.hops[1].base_mint);
        assert_eq!(sell_swap.accounts[21].pubkey, route.hops[0].base_mint);
    }

    #[test]
    fn curve_sol_buy_does_not_wrap() {
        let owner = Pubkey::new_unique();
        let mut first = hop(Market::Curve, 1_000_000, 1_000_000, false, false);
        first.quote_mint = *WSOL_MINT;
        let route = sample(vec![first], true);
        let buy = assemble(&owner, &route, true, 1_000, 10, None, 100_000, 1, 1).unwrap();
        assert!(
            buy.instructions
                .iter()
                .all(|ix| ix.program_id != system_program::id())
        );
        assert_eq!(
            buy.instructions
                .iter()
                .find(|ix| ix.program_id == *PUMP_AMM_PROGRAM)
                .unwrap()
                .accounts
                .len(),
            21
        );
    }

    #[test]
    fn external_leg_sits_on_the_sol_side_of_the_pump_hop() {
        let owner = Pubkey::new_unique();
        let route = sample(
            vec![hop(Market::Curve, 1_000_000, 1_000_000, false, false)],
            false,
        );
        let marker = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![],
            data: vec![7],
        };
        let leg = Leg {
            instructions: vec![marker],
            lookup_tables: vec![],
            min_out: 9,
        };
        let buy = assemble(&owner, &route, true, 9, 4, Some(leg), 100_000, 1, 1).unwrap();
        let jupiter = buy
            .instructions
            .iter()
            .position(|ix| ix.data == vec![7])
            .unwrap();
        let pump = buy
            .instructions
            .iter()
            .position(|ix| ix.program_id == *PUMP_AMM_PROGRAM)
            .unwrap();
        assert!(jupiter < pump);
        let leg = Leg {
            instructions: vec![Instruction {
                program_id: Pubkey::new_unique(),
                accounts: vec![],
                data: vec![7],
            }],
            lookup_tables: vec![],
            min_out: 1,
        };
        let sell = assemble(&owner, &route, false, 100, 9, Some(leg), 100_000, 1, 1).unwrap();
        let jupiter = sell
            .instructions
            .iter()
            .position(|ix| ix.data == vec![7])
            .unwrap();
        let pump = sell
            .instructions
            .iter()
            .position(|ix| ix.program_id == *PUMP_AMM_PROGRAM)
            .unwrap();
        assert!(pump < jupiter);
        assert_eq!(sell.compute_unit_limit, 1_400_000);
    }

    #[test]
    fn mayhem_curve_uses_quote_v3() {
        let owner = Pubkey::new_unique();
        let route = sample(
            vec![hop(Market::Curve, 1_000_000, 1_000_000, true, false)],
            false,
        );
        assert!(!multi_hop_allowed(&route));
        let buy = assemble(&owner, &route, true, 50, 10, None, 100_000, 1, 1).unwrap();
        let ix = buy
            .instructions
            .iter()
            .find(|ix| ix.program_id == *PUMP_PROGRAM)
            .unwrap();
        assert_eq!(&ix.data[..8], &BUY_EXACT_QUOTE_IN_V3);
        assert_eq!(ix.accounts.len(), 17);
        assert_eq!(*ix.data.last().unwrap(), 1);
        let nested = sample(
            vec![
                hop(Market::Curve, 1, 1, false, false),
                hop(Market::Curve, 1, 1, true, false),
            ],
            true,
        );
        assert!(assemble(&owner, &nested, true, 1, 1, None, 1, 1, 1).is_err());
    }
}
