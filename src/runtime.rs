use crate::alt_quote::AltQuote;
use crate::config::{AppConfig, StrategyName, journal_record_matches_strategy};
use crate::events::{
    PoolTradeEvent, Side, lamports_to_sol, pool_key, sol_to_lamports_f64, to_strategy_price,
};
use crate::execution::{LiveExecution, holding};
use crate::geyser::{GeyserCommand, GeyserHandle};
use crate::journal::RecoveryJournal;
use crate::state::{Lifecycle, TokenInner};
use crate::strategy::engine::Engine;
use crate::strategy::to_market_event;
use crate::strategy::v071_tape::V071Tape;
use crate::venues::decoder;
use crate::venues::ids::ata;
use anyhow::Result;
use solana_account_decoder_client_types::UiAccountData;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

struct StrategySlot {
    name: StrategyName,
    target_wallet: String,
    target_sell_exit: bool,
    first_cycle_only: bool,
    buy_amount_lamports: u64,
    quote_pairs: crate::config::QuotePairs,
    execution: Arc<LiveExecution>,
    tokens: Mutex<HashMap<String, Arc<Mutex<TokenInner>>>>,
    busy: Mutex<HashSet<String>>,
    pending_sells: Mutex<HashMap<String, String>>,
}

pub struct TradingRuntime {
    config: Arc<AppConfig>,
    rpc: Arc<RpcClient>,
    geyser: GeyserHandle,
    journal: Arc<RecoveryJournal>,
    quotes: Arc<AltQuote>,
    /// Print tape for v071 age and volume. Present when v071 is running.
    /// Age is firehose pump-create Instant stamps (no birth RPC).
    tape: Option<Arc<V071Tape>>,
    slots: Vec<Arc<StrategySlot>>,
    /// program:pool → (mint, set of slot names still needing the tape)
    registered: Mutex<HashMap<String, (Pubkey, HashSet<&'static str>)>>,
    inbound: Mutex<Option<mpsc::Receiver<crate::events::ParsedTargetTransaction>>>,
    received: std::sync::atomic::AtomicU64,
    decoded: std::sync::atomic::AtomicU64,
}

impl TradingRuntime {
    pub fn new(
        config: Arc<AppConfig>,
        rpc: Arc<RpcClient>,
        geyser: GeyserHandle,
        journal: Arc<RecoveryJournal>,
        quotes: Arc<AltQuote>,
        tape: Option<Arc<V071Tape>>,
        executions: Vec<Arc<LiveExecution>>,
        inbound: mpsc::Receiver<crate::events::ParsedTargetTransaction>,
    ) -> Arc<Self> {
        let slots = config
            .strategy_plans
            .iter()
            .zip(executions)
            .map(|(plan, execution)| {
                Arc::new(StrategySlot {
                    name: plan.name,
                    target_wallet: plan.target_wallet.clone(),
                    target_sell_exit: plan.target_sell_exit,
                    first_cycle_only: plan.first_cycle_only,
                    buy_amount_lamports: plan.buy_amount_lamports,
                    quote_pairs: plan.quote_pairs,
                    execution,
                    tokens: Mutex::new(HashMap::new()),
                    busy: Mutex::new(HashSet::new()),
                    pending_sells: Mutex::new(HashMap::new()),
                })
            })
            .collect();
        Arc::new(Self {
            config,
            rpc,
            geyser,
            journal,
            quotes,
            tape,
            slots,
            registered: Mutex::new(HashMap::new()),
            inbound: Mutex::new(Some(inbound)),
            received: std::sync::atomic::AtomicU64::new(0),
            decoded: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub async fn start(self: &Arc<Self>) -> Result<()> {
        for slot in &self.slots {
            tracing::info!(
                strategy = slot.name.as_str(),
                quote_pairs = slot.quote_pairs.as_str(),
                "[01 STARTUP] Quote pairs"
            );
        }
        tracing::info!("[02 STREAM] Geyser client started");
        self.recover().await?;
        let Some(mut rx) = self.inbound.lock().await.take() else {
            anyhow::bail!("inbound receiver already taken");
        };
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(tx) = rx.recv().await {
                runtime
                    .received
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Err(err) = runtime.handle(tx).await {
                    tracing::error!(error = %err, "[ERROR] Transaction processing failed");
                }
            }
        });

        let tick_ms = self
            .slots
            .iter()
            .map(|slot| match slot.name {
                StrategyName::V011 => self.config.strategy_v011.timer_ms.max(50),
                _ => 1_000,
            })
            .min()
            .unwrap_or(200)
            .max(50);
        let clock = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(tick_ms)).await;
                clock.on_clock().await;
            }
        });

        let health = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let mut strategies = Vec::new();
                for slot in &health.slots {
                    strategies.push((slot.name.as_str(), slot.tokens.lock().await.len()));
                }
                tracing::info!(
                    received_transactions = health.received.load(std::sync::atomic::Ordering::Relaxed),
                    decoded_events = health.decoded.load(std::sync::atomic::Ordering::Relaxed),
                    strategies = ?strategies,
                    "[HEALTH] Bot is running"
                );
            }
        });
        let targets: Vec<_> = self
            .slots
            .iter()
            .map(|slot| (slot.name.as_str(), slot.target_wallet.as_str()))
            .collect();
        tracing::info!(
            targets = ?targets,
            strategy = %self.config.strategy_label,
            timer_ms = tick_ms,
            "[01 STARTUP] Trading runtime started"
        );
        Ok(())
    }

    async fn on_clock(self: &Arc<Self>) {
        let now = now_ms();
        for slot in &self.slots {
            let tokens: Vec<_> = slot.tokens.lock().await.values().cloned().collect();
            for token in tokens {
                let (signal, buy_reason, sell_reason, key, strategy, needed, changed) = {
                    let mut state = token.lock().await;
                    if matches!(state.lifecycle, Lifecycle::Closed | Lifecycle::Failed) {
                        continue;
                    }
                    let mark = state
                        .prices
                        .current_mark_price
                        .map(to_strategy_price)
                        .unwrap_or(0.0);
                    let is_holding = holding(&state);
                    let signal = state.engine.on_clock(now, mark, is_holding);
                    let needed = state.engine.needs_pool_tape();
                    let changed = state.pool_tape_needed != Some(needed);
                    state.pool_tape_needed = Some(needed);
                    (
                        signal.map(|s| s.to_string()),
                        state.engine.last_buy_reason(),
                        state.engine.last_sell_reason(),
                        state.key(),
                        state.strategy_name.clone(),
                        needed,
                        changed,
                    )
                };
                if changed {
                    self.sync_pool_need(slot, &key, needed).await;
                }
                if let Some(signal) = signal {
                    tracing::info!(strategy, mint_key = %key, signal, "[04 STRAT] clock decision");
                    let runtime = Arc::clone(self);
                    let slot = Arc::clone(slot);
                    let token = Arc::clone(&token);
                    tokio::spawn(async move {
                        runtime
                            .dispatch(slot, token, &key, &signal, &buy_reason, &sell_reason)
                            .await;
                    });
                }
            }
        }
    }

    async fn handle(self: &Arc<Self>, tx: crate::events::ParsedTargetTransaction) -> Result<()> {
        let trades = decoder::decode(&tx);
        self.decoded
            .fetch_add(trades.len() as u64, std::sync::atomic::Ordering::Relaxed);
        for parsed in trades {
            let event = parsed.event;
            let descriptor = parsed.descriptor;
            if let Some(curve) = event.curve.clone() {
                // Shared venue cache — any slot's execution venues work.
                self.slots[0]
                    .execution
                    .venues
                    .note_curve(&descriptor.mint, curve);
            }
            if let Some(swap) = event.swap.clone() {
                self.slots[0]
                    .execution
                    .venues
                    .note_swap(&descriptor.mint, swap);
            }
            let mut created = Vec::new();
            for slot in &self.slots {
                if self.apply_slot(slot, &descriptor, &event).await? {
                    created.push(slot.name.as_str());
                }
            }
            if !created.is_empty() {
                tracing::info!(
                    mint = %descriptor.mint,
                    pool = %descriptor.pool,
                    strategies = ?created,
                    "[03 DETECT] Target bought token; now tracking pool"
                );
            }
        }
        Ok(())
    }

    /// SOL pairs pass through. A fresh Jupiter rate rewrites an alt-quote print
    /// into SOL. A missing rate keeps the print and refreshes in the background.
    /// An open pair keeps its last mark so a target sell still fires.
    ///
    /// Never awaits RPC/HTTP — FX and mint meta refresh off-thread so SOL
    /// decisions on the serial inbound loop stay fast.
    async fn strategy_event(
        self: &Arc<Self>,
        slot: &StrategySlot,
        key: &str,
        descriptor: &crate::events::PoolDescriptor,
        event: &PoolTradeEvent,
    ) -> Option<PoolTradeEvent> {
        let Some(quote_mint) = pump_alt_mint(descriptor, event) else {
            return Some(event.clone());
        };
        let mut priced = event.clone();
        if descriptor.venue == "launchlab" {
            self.spawn_cache_launchpad(descriptor.clone());
        }
        if self
            .quotes
            .scale_from_cache(&quote_mint, &mut priced.sol_amount, &mut priced.price)
        {
            return Some(priced);
        }
        self.quotes.spawn_quote_ready(quote_mint);
        self.keep_unpriced_alt_print(slot, key, descriptor, priced, None)
            .await
    }

    /// Missing SOL rate. An open pair keeps its last mark and ignores this
    /// print's size. A new pair is still tracked, with the quote amount left
    /// as it arrived, until a later print has a rate.
    async fn keep_unpriced_alt_print(
        &self,
        slot: &StrategySlot,
        key: &str,
        descriptor: &crate::events::PoolDescriptor,
        mut priced: PoolTradeEvent,
        err: Option<anyhow::Error>,
    ) -> Option<PoolTradeEvent> {
        let existing = slot.tokens.lock().await.get(key).cloned();
        if let Some(token) = existing {
            tracing::warn!(
                error = err.as_ref().map(|err| err.to_string()).unwrap_or_default(),
                mint = %descriptor.mint,
                "[03 DETECT] Alt-quote FX unavailable; keeping last mark"
            );
            priced.sol_amount = 0;
            if let Some(mark) = token.lock().await.prices.current_mark_price {
                priced.price = mark;
            }
            return Some(priced);
        }
        tracing::warn!(
            error = err.as_ref().map(|err| err.to_string()).unwrap_or_default(),
            mint = %descriptor.mint,
            "[03 DETECT] Alt-quote FX missing; keeping the print and refreshing the rate"
        );
        Some(priced)
    }

    async fn apply_slot(
        self: &Arc<Self>,
        slot: &Arc<StrategySlot>,
        descriptor: &crate::events::PoolDescriptor,
        event: &PoolTradeEvent,
    ) -> Result<bool> {
        let key = pool_key(&descriptor.program_id, &descriptor.pool);
        let quote_atoms = event.sol_amount;
        let Some(priced) = self.strategy_event(slot, &key, descriptor, event).await else {
            return Ok(false);
        };
        let event = &priced;
        let existing = slot.tokens.lock().await.get(&key).cloned();
        let token = if let Some(token) = existing {
            token
        } else if event.trader == slot.target_wallet && event.side == Side::Buy {
            let quote_mint = pair_quote_mint(descriptor, event);
            if let Some(code) = slot.quote_pairs.rejects(quote_mint) {
                tracing::info!(
                    strategy = slot.name.as_str(),
                    mint = %descriptor.mint,
                    quote_mint = quote_mint.unwrap_or(""),
                    reason = code,
                    "[03 DETECT] Skipping pair for this strategy"
                );
                return Ok(false);
            }
            if !sol_pair(descriptor, event) {
                let Some(alt_mint) = pump_alt_mint(descriptor, event) else {
                    tracing::info!(
                        mint = %descriptor.mint,
                        quote_mint = descriptor.quote_mint.as_deref().unwrap_or(""),
                        "[03 DETECT] Skipping non-SOL pair"
                    );
                    return Ok(false);
                };
                // Warm FX + mint meta before the next prints hit the seek window.
                self.quotes.spawn_quote_ready(alt_mint);
                tracing::info!(
                    mint = %descriptor.mint,
                    quote_mint = descriptor.quote_mint.as_deref().unwrap_or(""),
                    venue = %descriptor.venue,
                    "[03 DETECT] Alt-quote pair"
                );
            }
            let mut engine = self.make_engine(slot);
            engine.set_v071_sol_quote(sol_pair(descriptor, event));
            // Gate buy itself is the cycle open — freeze age/volume to this print.
            let tape_sample = self.v071_tape_sample(
                slot,
                descriptor,
                event,
                &event.signature,
                event.timestamp_ms,
            );
            let token = Arc::new(Mutex::new(TokenInner::new(
                descriptor.clone(),
                event.clone(),
                (self.config.event_retention_sec * 1000.0) as u64,
                engine,
            )));
            {
                let mut state = token.lock().await;
                state.transition(Lifecycle::TrackingPool)?;
                state.prices.current_mark_price = Some(event.price);
                let price = to_strategy_price(event.price);
                let sol = lamports_to_sol(event.sol_amount);
                if let Some((volume, age)) = tape_sample {
                    state.engine.set_v071_open_tape(volume, age);
                }
                state.engine.bind_gate(
                    &slot.target_wallet,
                    event.token_amount as f64,
                    price,
                    sol,
                    &event.signature,
                    event.timestamp_ms,
                );
                state.pool_tape_needed = Some(true);
            }
            slot.tokens
                .lock()
                .await
                .insert(key.clone(), Arc::clone(&token));
            self.mark_pool_needed(slot, &key, descriptor).await;
            // Gate buy only binds; later prints / timers decide entry.
            return Ok(true);
        } else {
            return Ok(false);
        };

        let own_wallet = slot.execution.wallet.pubkey().to_string();
        let tape_sample = if slot.name == StrategyName::V071 {
            let prefetch = {
                let state = token.lock().await;
                let gate = event.signature == state.target_buy.signature
                    && event.event_index == state.target_buy.event_index;
                let target_buy = event.trader == slot.target_wallet && event.side == Side::Buy;
                // Re-sample while seeking with a blocked tape so background
                // Jupiter FX (volume) can unlock without awaiting on detect.
                let retry_blocked = state.engine.v071_needs_tape_refresh();
                let should = !gate && (!state.engine.is_bound() || target_buy || retry_blocked);
                if !should {
                    None
                } else if target_buy {
                    // New cycle opens on this target buy — freeze to this print.
                    Some((event.signature.clone(), event.timestamp_ms))
                } else {
                    // FX retry (or late bind): keep the recorded gate buy so a
                    // later pool print cannot inflate age/volume.
                    Some((
                        state.target_buy.signature.clone(),
                        state.target_buy.timestamp_ms,
                    ))
                }
            };
            match prefetch {
                Some((open_sig, open_ts_ms)) => {
                    self.v071_tape_sample(slot, descriptor, event, &open_sig, open_ts_ms)
                }
                None => None,
            }
        } else {
            None
        };
        let (signal, buy_reason, sell_reason, needed, changed, strategy) = {
            let mut state = token.lock().await;
            if event.trader == own_wallet {
                state.note_own_fill(&event.signature, event.token_amount, quote_atoms);
            }
            if event.trader == slot.target_wallet {
                state.record_target_trade(event);
            }
            if !state.events.add(event.clone()) {
                return Ok(false);
            }
            state.prices.current_mark_price = Some(event.price);
            let gate = event.signature == state.target_buy.signature
                && event.event_index == state.target_buy.event_index;
            if gate {
                if !state.engine.is_bound() {
                    let price = to_strategy_price(event.price);
                    let sol = lamports_to_sol(event.sol_amount);
                    state.engine.bind_gate(
                        &slot.target_wallet,
                        if event.side == Side::Buy {
                            event.token_amount as f64
                        } else {
                            0.0
                        },
                        price,
                        sol,
                        &event.signature,
                        event.timestamp_ms,
                    );
                }
                let needed = state.engine.needs_pool_tape();
                let changed = state.pool_tape_needed != Some(needed);
                state.pool_tape_needed = Some(needed);
                drop(state);
                if changed {
                    self.sync_pool_need(slot, &key, needed).await;
                }
                return Ok(false);
            }
            if !state.engine.is_bound() {
                let price = to_strategy_price(state.target_buy.price);
                let sol = lamports_to_sol(state.target_buy.sol_amount);
                let gate_sig = state.target_buy.signature.clone();
                let gate_ts = state.target_buy.timestamp_ms;
                if let Some((volume, age)) = tape_sample {
                    state.engine.set_v071_open_tape(volume, age);
                }
                state
                    .engine
                    .bind_gate(&slot.target_wallet, 0.0, price, sol, &gate_sig, gate_ts);
            } else if let Some((volume, age)) = tape_sample {
                // Target rebuy, or a blocked seek waiting on background birth/FX.
                state.engine.set_v071_open_tape(volume, age);
            }
            let before = state.engine.phase_name();
            let market = to_market_event(event);
            let is_holding = holding(&state);
            let signal = state.engine.on_event(&market, is_holding);
            let after = state.engine.phase_name();
            let cycle_ended = after == "idle" || after == "done";
            if before == "pending"
                && cycle_ended
                && market.side == "SELL"
                && market.wallet == slot.target_wallet
                && (slot.target_sell_exit || slot.first_cycle_only)
            {
                state.sell_after_fill = true;
            }
            let needed = state.engine.needs_pool_tape();
            let changed = state.pool_tape_needed != Some(needed);
            state.pool_tape_needed = Some(needed);
            (
                signal.map(|s| s.to_string()),
                state.engine.last_buy_reason(),
                state.engine.last_sell_reason(),
                needed,
                changed,
                state.strategy_name.clone(),
            )
        };
        if changed {
            self.sync_pool_need(slot, &key, needed).await;
        }
        if let Some(signal) = signal {
            tracing::info!(strategy, mint = %descriptor.mint, signal, "[04 STRAT] decision");
            let runtime = Arc::clone(self);
            let slot = Arc::clone(slot);
            let token = Arc::clone(&token);
            tokio::spawn(async move {
                runtime
                    .dispatch(slot, token, &key, &signal, &buy_reason, &sell_reason)
                    .await;
            });
        }
        Ok(false)
    }

    fn make_engine(&self, slot: &StrategySlot) -> Engine {
        let own = slot.execution.wallet.pubkey().to_string();
        match slot.name {
            StrategyName::V011 => Engine::new_v011(self.config.strategy_v011.clone()),
            StrategyName::V022 => Engine::new_v022(self.config.strategy_v022.clone(), own),
            StrategyName::V031 => Engine::new_v031(self.config.strategy_v031.clone(), own),
            StrategyName::V051 => Engine::new_v051(self.config.strategy_v051.clone(), own),
            StrategyName::V061 => Engine::new_v061(self.config.strategy_v061.clone(), own),
            StrategyName::V071 => Engine::new_v071(self.config.strategy_v071.clone(), own),
            StrategyName::V081 => Engine::new_v081(self.config.strategy_v081.clone(), own),
            StrategyName::V101 => Engine::new_v101(self.config.strategy_v101.clone(), own),
            StrategyName::V121 => Engine::new_v121(self.config.strategy_v121.clone(), own),
            StrategyName::V131 => Engine::new_v131(self.config.strategy_v131.clone(), own),
        }
    }

    /// Buy SOL plus sell SOL through the cycle-open buy, and seconds from the
    /// firehose pump-create stamp to that same buy (sibling project `1` age
    /// getting — Instant launch map, no birth RPC). `open_signature` must be
    /// the gate buy that opened the cycle so volume/age freeze there.
    /// SOL pairs convert the lookback to SOL. Non-SOL pairs skip the volume
    /// band (Jupiter FX would otherwise block seek); age still gates.
    /// Missing FX refreshes in the background and never blocks the serial
    /// inbound handler.
    fn v071_tape_sample(
        self: &Arc<Self>,
        slot: &StrategySlot,
        descriptor: &crate::events::PoolDescriptor,
        event: &PoolTradeEvent,
        open_signature: &str,
        _open_timestamp_ms: u64,
    ) -> Option<(Option<f64>, Option<f64>)> {
        if slot.name != StrategyName::V071 {
            return None;
        }
        let cfg = &self.config.strategy_v071;
        let age_gate = cfg.min_target_age_s > 0.0 || cfg.max_target_age_s > 0.0;
        // Alt-quote volume needs Jupiter FX and often stalls entry; skip that
        // band for non-SOL while keeping the SOL lookback gate.
        let volume_gate = sol_pair(descriptor, event)
            && cfg.target_volume_s > 0.0
            && (cfg.min_target_volume_sol > 0.0 || cfg.max_target_volume_sol > 0.0);
        // `None` volume skips the band — only safe when the volume gate is off
        // (non-SOL or config). Unknown samples must be NaN so SOL still blocks.
        let unknown = || {
            Some((
                if volume_gate { Some(f64::NAN) } else { None },
                if age_gate { Some(f64::NAN) } else { None },
            ))
        };
        let Some(tape) = self.tape.as_ref() else {
            return unknown();
        };
        let Ok(mint) = event.mint.parse::<Pubkey>() else {
            return unknown();
        };
        let signature = open_signature
            .parse::<solana_sdk::signature::Signature>()
            .ok();
        let lookback = std::time::Duration::from_secs_f64(cfg.target_volume_s.max(0.0));
        let (legs, unpriced, tape_age) = tape.open_window(&mint, lookback, signature.as_ref());
        // Missing create → NaN (unknown), same as project 1 skipping arm.
        let age = if age_gate { Some(tape_age) } else { None };
        let volume = if !volume_gate {
            None
        } else if unpriced {
            tracing::warn!(
                mint = %event.mint,
                "[04 STRAT] v071 alt-quote volume has no quote mint"
            );
            Some(f64::NAN)
        } else {
            match self.volume_legs_to_sol(&legs) {
                Some(sol) => Some(sol),
                None => {
                    tracing::warn!(
                        mint = %event.mint,
                        "[04 STRAT] v071 alt-quote volume has no SOL rate"
                    );
                    Some(f64::NAN)
                }
            }
        };
        tracing::info!(
            mint = %event.mint,
            volume = volume.filter(|sol| sol.is_finite()).unwrap_or(-1.0),
            age_sec = age.filter(|sec| sec.is_finite()).unwrap_or(-1.0),
            skip_volume = !volume_gate && !sol_pair(descriptor, event),
            "[04 STRAT] v071 tape"
        );
        Some((volume, age))
    }

    /// Sum window legs as SOL from cache only. Missing FX/meta refreshes in the
    /// background and makes the whole sum unknown — never awaits Jupiter/RPC.
    fn volume_legs_to_sol(&self, legs: &[(Option<Pubkey>, u64)]) -> Option<f64> {
        let mut sol = 0.0;
        for (quote, atoms) in legs {
            let part = match quote {
                None => *atoms as f64 / 1_000_000_000.0,
                Some(quote) => match self.quotes.quote_atoms_to_sol_cached(quote, *atoms) {
                    Some(part) => part,
                    None => {
                        self.quotes.spawn_quote_ready(*quote);
                        return None;
                    }
                },
            };
            if !part.is_finite() {
                return None;
            }
            sol += part;
        }
        sol.is_finite().then_some(sol)
    }

    async fn mark_pool_needed(
        self: &Arc<Self>,
        slot: &StrategySlot,
        key: &str,
        descriptor: &crate::events::PoolDescriptor,
    ) {
        {
            let mut registered = self.registered.lock().await;
            let entry = registered
                .entry(key.to_string())
                .or_insert_with(|| (Pubkey::default(), HashSet::new()));
            entry.1.insert(slot.name.as_str());
        }
        self.enqueue_subscribe(descriptor.clone());
    }

    async fn sync_pool_need(self: &Arc<Self>, slot: &StrategySlot, key: &str, needed: bool) {
        let should_unsubscribe = {
            let mut registered = self.registered.lock().await;
            if needed {
                if let Some((_, holders)) = registered.get_mut(key) {
                    holders.insert(slot.name.as_str());
                }
                false
            } else if let Some((_, holders)) = registered.get_mut(key) {
                holders.remove(slot.name.as_str());
                holders.is_empty()
            } else {
                false
            }
        };
        if should_unsubscribe {
            let runtime = Arc::clone(self);
            let key = key.to_string();
            tokio::spawn(async move {
                runtime.release_pool(&key).await;
            });
        }
    }

    async fn claim_busy(&self, slot: &StrategySlot, key: &str) -> bool {
        let mut busy = slot.busy.lock().await;
        if busy.contains(key) {
            return false;
        }
        busy.insert(key.to_string());
        true
    }

    async fn release_busy(&self, slot: &StrategySlot, key: &str) -> Option<String> {
        slot.busy.lock().await.remove(key);
        slot.pending_sells.lock().await.remove(key)
    }

    fn spawn_pending_sell(
        self: &Arc<Self>,
        slot: Arc<StrategySlot>,
        token: Arc<Mutex<TokenInner>>,
        key: &str,
        reason: String,
    ) {
        let runtime = Arc::clone(self);
        let key = key.to_string();
        tokio::spawn(async move {
            if runtime.claim_busy(&slot, &key).await {
                runtime.sell(slot, token, &key, &reason).await;
            }
        });
    }

    fn enqueue_subscribe(self: &Arc<Self>, descriptor: crate::events::PoolDescriptor) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = runtime.subscribe_pool(&descriptor).await {
                tracing::warn!(error = %err, mint = %descriptor.mint, "[02 STREAM] Pool subscribe failed");
            }
        });
    }

    async fn dispatch(
        self: &Arc<Self>,
        slot: Arc<StrategySlot>,
        token: Arc<Mutex<TokenInner>>,
        key: &str,
        signal: &str,
        buy_reason: &str,
        sell_reason: &str,
    ) {
        if !self.claim_busy(&slot, key).await {
            if signal == "SELL" {
                slot.pending_sells
                    .lock()
                    .await
                    .insert(key.to_string(), sell_reason.to_string());
                tracing::info!(%key, reason = sell_reason, "[08 EXIT] Queued sell while busy");
            }
            return;
        }
        if signal == "BUY" {
            self.buy(slot, token, key, buy_reason).await;
        } else if signal == "SELL" {
            self.sell(slot, token, key, sell_reason).await;
        } else if let Some(reason) = self.release_busy(&slot, key).await {
            self.spawn_pending_sell(slot, token, key, reason);
        }
    }

    async fn buy(
        self: &Arc<Self>,
        slot: Arc<StrategySlot>,
        token: Arc<Mutex<TokenInner>>,
        key: &str,
        reason: &str,
    ) {
        let sent = self.buy_inner(&slot, &token, reason).await;
        let follow = {
            let state = token.lock().await;
            sent && state.sell_after_fill && holding(&state)
        };
        if follow {
            let sell_reason = {
                let mut state = token.lock().await;
                state.sell_after_fill = false;
                state.engine.last_sell_reason()
            };
            slot.pending_sells.lock().await.remove(key);
            let reason = if sell_reason.is_empty() {
                "target_sell".to_string()
            } else {
                sell_reason
            };
            self.sell(slot, token, key, &reason).await;
            return;
        }
        if let Some(reason) = self.release_busy(&slot, key).await {
            self.spawn_pending_sell(slot, token, key, reason);
        }
    }

    async fn buy_inner(
        &self,
        slot: &StrategySlot,
        token: &Mutex<TokenInner>,
        reason: &str,
    ) -> bool {
        {
            let mut state = token.lock().await;
            if holding(&state) || !state.claim_buy_send() {
                return false;
            }
            if !self.can_open(slot, &state) {
                state.sell_after_fill = false;
                state.reset_buy_send_claim();
                state.engine.on_buy_failed();
                slot.execution.journal.record(serde_json::json!({
                    "event": "entry_blocked",
                    "strategy": state.strategy_name,
                    "descriptor": state.descriptor,
                    "lifecycle": state.lifecycle.as_str(),
                    "reason": reason,
                }));
                return false;
            }
            let size = state.engine.buy_size_sol();
            state.buy_lamports = Some(sol_to_lamports_f64(size));
            state.prices.entry_signal_price = state
                .prices
                .current_mark_price
                .or(Some(state.target_buy.price));
            state.buy_slippage_bps = Some(self.config.buy_slippage_bps);
            state.sell_slippage_bps = Some(self.config.sell_slippage_bps);
            let strategy = state.strategy_name.clone();
            tracing::info!(strategy, mint = %state.descriptor.mint, reason, size_sol = size, "[05 ENTRY] buy signal");
            let owner = slot.execution.wallet.pubkey();
            let mint = state.descriptor.mint.clone();
            let venue = state.descriptor.venue.clone();
            let token_program = state
                .descriptor
                .token_program
                .clone()
                .unwrap_or_else(|| (*crate::venues::ids::TOKEN_PROGRAM).to_string());
            let token_program = token_program
                .parse()
                .unwrap_or(*crate::venues::ids::TOKEN_PROGRAM);
            match slot.execution.venues.build_buy(
                &venue,
                &owner,
                &mint,
                &token_program,
                state.buy_lamports.unwrap_or(0),
                self.config.buy_slippage_bps,
            ) {
                Ok(prepared) => {
                    state.prepared_buy = Some(prepared);
                    if let Err(err) = state.transition(Lifecycle::BuyPrepared) {
                        tracing::error!(error = %err, strategy, "[05 ENTRY] buy failed");
                        state.engine.on_buy_failed();
                        state.reset_buy_send_claim();
                        return false;
                    }
                }
                Err(err) => {
                    tracing::error!(error = %err, strategy, mint = %mint, "[05 ENTRY] buy failed");
                    state.sell_after_fill = false;
                    state.engine.on_buy_failed();
                    state.reset_buy_send_claim();
                    return false;
                }
            }
        }
        if let Err(err) = slot.execution.send_buy(token).await {
            tracing::error!(error = %err, "[05 ENTRY] buy failed");
            let mut state = token.lock().await;
            state.sell_after_fill = false;
            state.engine.on_buy_failed();
            state.reset_buy_send_claim();
            if state.lifecycle == Lifecycle::BuyPrepared {
                let _ = state.transition(Lifecycle::TrackingPool);
            }
        }
        true
    }

    async fn sell(
        self: &Arc<Self>,
        slot: Arc<StrategySlot>,
        token: Arc<Mutex<TokenInner>>,
        key: &str,
        reason: &str,
    ) {
        {
            let mut state = token.lock().await;
            if !holding(&state) || !state.claim_sell_send() {
                drop(state);
                if let Some(reason) = self.release_busy(&slot, key).await {
                    self.spawn_pending_sell(slot, token, key, reason);
                }
                return;
            }
            state.prices.exit_signal_price = state.prices.current_mark_price;
            tracing::info!(strategy = %state.strategy_name, mint = %state.descriptor.mint, reason, "[08 EXIT] sell signal");
        }
        if let Err(err) = slot.execution.send_sell(&token, reason).await {
            tracing::error!(error = %err, "[08 EXIT] sell failed");
            let mut state = token.lock().await;
            state.release_sell_send();
            state.engine.on_sell_failed(now_ms());
        }
        if let Some(reason) = self.release_busy(&slot, key).await {
            self.spawn_pending_sell(slot, token, key, reason);
        }
    }

    fn can_open(&self, slot: &StrategySlot, state: &TokenInner) -> bool {
        let blocked: Vec<&str> = self
            .config
            .blocked_mints
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if blocked.iter().any(|mint| *mint == state.descriptor.mint) {
            tracing::warn!(mint = %state.descriptor.mint, "[05 ENTRY] Buy blocked: mint denylist");
            return false;
        }
        let quote_mint = state.descriptor.quote_mint.as_deref();
        if let Some(code) = slot.quote_pairs.rejects(quote_mint) {
            tracing::info!(
                strategy = slot.name.as_str(),
                mint = %state.descriptor.mint,
                quote_mint = quote_mint.unwrap_or(""),
                reason = code,
                "[05 ENTRY] Buy skipped: quote pair mode"
            );
            return false;
        }
        if !crate::venues::ids::is_sol_quote(quote_mint) && !alt_venue(&state.descriptor.venue) {
            tracing::info!(
                mint = %state.descriptor.mint,
                quote_mint = quote_mint.unwrap_or(""),
                "[05 ENTRY] Buy skipped: non-SOL pair"
            );
            return false;
        }
        true
    }

    async fn subscribe_pool(&self, descriptor: &crate::events::PoolDescriptor) -> Result<()> {
        let key = pool_key(&descriptor.program_id, &descriptor.pool);
        let mint: Pubkey = descriptor.mint.parse()?;
        {
            let reg = self.registered.lock().await;
            // `Pubkey::default()` means holders exist but Register has not been sent yet.
            if let Some((existing, _)) = reg.get(&key) {
                if *existing == mint {
                    return Ok(());
                }
            }
        }
        tracing::info!(mint = %descriptor.mint, pool = %descriptor.pool, venue = %descriptor.venue, "[02 STREAM] Opening pool subscription");
        let cmd = if descriptor.venue == "launchlab" {
            let pool: Pubkey = descriptor.pool.parse()?;
            GeyserCommand::RegisterCurve {
                mint,
                bonding_curve: pool,
                associated_bonding_curve: pool,
                associated_quote_bonding_curve: None,
            }
        } else if descriptor.venue == "pumpswap" {
            let (base, quote) = self.resolve_amm_vaults(descriptor).await?;
            GeyserCommand::RegisterAmm {
                mint,
                pool: descriptor.pool.parse()?,
                pool_base_token_account: base,
                pool_quote_token_account: quote,
            }
        } else {
            let bonding_curve: Pubkey = descriptor.pool.parse()?;
            let token_program: Pubkey = descriptor
                .token_program
                .as_deref()
                .unwrap_or(&crate::venues::ids::TOKEN_PROGRAM.to_string())
                .parse()
                .unwrap_or(*crate::venues::ids::TOKEN_PROGRAM);
            let associated_bonding_curve = ata(&bonding_curve, &mint, &token_program);
            let associated_quote_bonding_curve =
                if crate::venues::ids::is_sol_quote(descriptor.quote_mint.as_deref()) {
                    None
                } else {
                    match descriptor
                        .quote_mint
                        .as_deref()
                        .and_then(|q| q.parse().ok())
                    {
                        Some(quote_mint) => {
                            let quote_program = self
                                .quotes
                                .token_program(&quote_mint)
                                .await
                                .unwrap_or(*crate::venues::ids::TOKEN_PROGRAM);
                            Some(ata(&bonding_curve, &quote_mint, &quote_program))
                        }
                        None => None,
                    }
                };
            GeyserCommand::RegisterCurve {
                mint,
                bonding_curve,
                associated_bonding_curve,
                associated_quote_bonding_curve,
            }
        };
        let send = match self.geyser.commands.try_send(cmd) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(cmd)) => self
                .geyser
                .commands
                .send(cmd)
                .await
                .map_err(|e| anyhow::anyhow!(e)),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                anyhow::bail!("geyser command channel closed")
            }
        };
        send?;
        let mut reg = self.registered.lock().await;
        if let Some((stored, holders)) = reg.get_mut(&key) {
            *stored = mint;
            let _ = holders;
        } else {
            reg.insert(key, (mint, HashSet::new()));
        }
        Ok(())
    }

    async fn resolve_amm_vaults(
        &self,
        descriptor: &crate::events::PoolDescriptor,
    ) -> Result<(Pubkey, Pubkey)> {
        if let (Some(base), Some(quote)) = (
            &descriptor.pool_base_token_account,
            &descriptor.pool_quote_token_account,
        ) {
            return Ok((base.parse()?, quote.parse()?));
        }
        if let Some(swap) = self.slots[0].execution.venues.get_swap(&descriptor.mint) {
            return Ok((
                swap.pool_base_token_account.parse()?,
                swap.pool_quote_token_account.parse()?,
            ));
        }
        let pool: Pubkey = descriptor.pool.parse()?;
        let account = self.rpc.get_account(&pool).await?;
        let parsed = crate::venues::ids::parse_amm_pool(&account.data)
            .ok_or_else(|| anyhow::anyhow!("unable to parse pumpswap pool {}", descriptor.pool))?;
        Ok((
            parsed.pool_base_token_account,
            parsed.pool_quote_token_account,
        ))
    }

    async fn release_pool(&self, key: &str) {
        let Some((mint, holders)) = self.registered.lock().await.remove(key) else {
            return;
        };
        if !holders.is_empty() {
            // Race: someone re-needed it; put back.
            self.registered
                .lock()
                .await
                .insert(key.to_string(), (mint, holders));
            return;
        }
        if mint == Pubkey::default() {
            return;
        }
        self.abandon_alt_quotes(key).await;
        if let Err(err) = self
            .geyser
            .commands
            .send(GeyserCommand::Unregister { mint })
            .await
        {
            tracing::warn!(error = %err, %key, "[02 STREAM] Failed to unregister pool from geyser");
            return;
        }
        tracing::info!(%key, "[02 STREAM] Released pool from gRPC filter");
    }

    /// No slot still needs the tape. If a wallet is flat, swap leftover quote now and again in 8s.
    async fn abandon_alt_quotes(&self, key: &str) {
        for slot in &self.slots {
            let Some(token) = slot.tokens.lock().await.get(key).cloned() else {
                continue;
            };
            let state = token.lock().await;
            if holding(&state) || state.had_own_fill || !alt_venue(&state.descriptor.venue) {
                continue;
            }
            let Some(quote) = state
                .descriptor
                .quote_mint
                .as_deref()
                .filter(|quote| !crate::venues::ids::is_sol_quote(Some(quote)))
                .and_then(|quote| quote.parse().ok())
            else {
                continue;
            };
            drop(state);
            Arc::clone(&slot.execution).spawn_abandon_reclaim(quote);
        }
    }

    /// Kick LaunchLab pool RPC off the detect loop when the mint is not cached yet.
    fn spawn_cache_launchpad(self: &Arc<Self>, descriptor: crate::events::PoolDescriptor) {
        if self.slots[0]
            .execution
            .venues
            .get_launchpad(&descriptor.mint)
            .is_some()
        {
            return;
        }
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            runtime.cache_launchpad(&descriptor).await;
        });
    }

    async fn cache_launchpad(&self, descriptor: &crate::events::PoolDescriptor) {
        if self.slots[0]
            .execution
            .venues
            .get_launchpad(&descriptor.mint)
            .is_some()
        {
            return;
        }
        let Ok(key) = descriptor.pool.parse::<Pubkey>() else {
            return;
        };
        let Ok(account) = self.rpc.get_account(&key).await else {
            return;
        };
        let Ok(state) =
            crate::venues::launchpad::LaunchpadPoolState::from_account_data(&key, &account.data)
        else {
            return;
        };
        if state.completed() {
            self.slots[0]
                .execution
                .venues
                .mark_curve_complete(&descriptor.mint);
        }
        self.slots[0]
            .execution
            .venues
            .note_launchpad(&descriptor.mint, state);
    }

    async fn recover(&self) -> Result<()> {
        let records = self.journal.read().await?;
        let active: Vec<StrategyName> = self.slots.iter().map(|s| s.name).collect();
        let mut latest: HashMap<(StrategyName, String), crate::journal::JournalRecord> =
            HashMap::new();
        let mut last_buy_fill: HashMap<(StrategyName, String), String> = HashMap::new();
        let mut last_buy_sig: HashMap<(StrategyName, String), String> = HashMap::new();
        for record in records {
            let Some(descriptor) = record.descriptor.clone() else {
                continue;
            };
            let key = pool_key(&descriptor.program_id, &descriptor.pool);
            let Some(slot) = self.slots.iter().find(|slot| {
                journal_record_matches_strategy(record.strategy.as_deref(), slot.name, &active)
            }) else {
                continue;
            };
            let sk = (slot.name, key.clone());
            if record.event.as_deref() == Some("position_closed") {
                last_buy_fill.remove(&sk);
                last_buy_sig.remove(&sk);
            } else {
                if record.event.as_deref() == Some("buy_processed") {
                    if let Some(sol) = record.fill.as_ref().and_then(|f| f.sol_amount.clone()) {
                        last_buy_fill.insert(sk.clone(), sol);
                    }
                }
                if record.event.as_deref() == Some("buy_sent") {
                    if let Some(sig) = record.signature.clone().or(record.buy_signature.clone()) {
                        last_buy_sig.insert(sk.clone(), sig);
                    }
                }
            }
            latest.insert(sk, record);
        }
        for ((strategy_name, key), record) in latest {
            let Some(slot) = self.slots.iter().find(|s| s.name == strategy_name) else {
                continue;
            };
            let Some(descriptor) = record.descriptor.clone() else {
                continue;
            };
            if matches!(record.lifecycle.as_deref(), Some("CLOSED" | "FAILED")) {
                continue;
            }
            let quote_mint = descriptor.quote_mint.as_deref();
            if let Some(code) = slot.quote_pairs.rejects(quote_mint) {
                tracing::info!(
                    strategy = slot.name.as_str(),
                    mint = %descriptor.mint,
                    quote_mint = quote_mint.unwrap_or(""),
                    reason = code,
                    "[07 POSITION] Skipping recovery: quote pair mode"
                );
                continue;
            }
            if !crate::venues::ids::is_sol_quote(quote_mint) && !alt_venue(&descriptor.venue) {
                tracing::info!(
                    mint = %descriptor.mint,
                    quote_mint = quote_mint.unwrap_or(""),
                    "[07 POSITION] Skipping recovery of non-SOL pair"
                );
                continue;
            }
            let buy_sig = record
                .buy_signature
                .clone()
                .or_else(|| last_buy_sig.get(&(strategy_name, key.clone())).cloned())
                .or_else(|| record.signature.clone());
            let mut recorded = record
                .actual_token_amount
                .as_deref()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let mut entry_price = record
                .prices
                .as_ref()
                .and_then(|p| p.actual_entry_fill_price)
                .or_else(|| record.fill.as_ref().and_then(|f| f.price))
                .unwrap_or(0.0);
            let mut fill_sol: Option<u64> = record
                .actual_entry_sol_amount
                .clone()
                .or_else(|| last_buy_fill.get(&(strategy_name, key.clone())).cloned())
                .and_then(|v| v.parse().ok());
            if (recorded == 0 || entry_price <= 0.0)
                && let Some(sig) = buy_sig.as_deref()
            {
                match slot
                    .execution
                    .fill_from_signature(sig, &descriptor.mint, &descriptor.venue)
                    .await
                {
                    Ok(Some((token_amount, sol_amount, price))) => {
                        if recorded == 0 {
                            recorded = token_amount;
                        }
                        if entry_price <= 0.0 {
                            entry_price = price;
                        }
                        if fill_sol.is_none() {
                            fill_sol = Some(sol_amount);
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!(error = %err, signature = %sig, mint = %descriptor.mint, "[07 POSITION] Recovery fill lookup failed");
                    }
                }
            }
            let mint: Pubkey = descriptor.mint.parse()?;
            let accounts = self
                .rpc
                .get_token_accounts_by_owner(
                    &slot.execution.wallet.pubkey(),
                    solana_client::rpc_request::TokenAccountsFilter::Mint(mint),
                )
                .await?;
            let mut wallet_amount = 0u64;
            for keyed in accounts {
                if let UiAccountData::Json(parsed) = keyed.account.data {
                    if let Some(amount) = parsed
                        .parsed
                        .get("info")
                        .and_then(|info| info.get("tokenAmount"))
                        .and_then(|amount| amount.get("amount"))
                        .and_then(|v| v.as_str())
                        .and_then(|v| v.parse().ok())
                    {
                        wallet_amount = wallet_amount.saturating_add(amount);
                    }
                }
            }
            let token_amount = if recorded == 0 {
                wallet_amount
            } else {
                recorded.min(wallet_amount)
            };
            if token_amount == 0 || entry_price <= 0.0 {
                continue;
            }
            let seed = PoolTradeEvent {
                signature: buy_sig.clone().unwrap_or_else(|| "recovered".to_string()),
                slot: 0,
                transaction_index: None,
                event_index: 0,
                timestamp_ms: record.entry_processed_ms.unwrap_or_else(now_ms),
                received_mono_ms: crate::events::mono_ms(),
                mint: descriptor.mint.clone(),
                pool: descriptor.pool.clone(),
                program_id: descriptor.program_id.clone(),
                trader: slot.target_wallet.clone(),
                side: Side::Buy,
                sol_amount: 0,
                token_amount,
                price: entry_price,
                curve_progress: if descriptor.venue == "pumpswap" {
                    Some(1.0)
                } else {
                    None
                },
                curve: None,
                swap: None,
            };
            let mut engine = self.make_engine(slot);
            engine.set_v071_sol_quote(crate::venues::ids::is_sol_quote(quote_mint));
            let strat_px = to_strategy_price(entry_price);
            engine.bind_gate(
                &slot.target_wallet,
                0.0,
                strat_px,
                0.0,
                &seed.signature,
                seed.timestamp_ms,
            );
            engine.restore_hold(strat_px, record.entry_processed_ms.unwrap_or_else(now_ms));
            let mut state = TokenInner::new(
                descriptor.clone(),
                seed,
                (self.config.event_retention_sec * 1000.0) as u64,
                engine,
            );
            state.restore_position(
                token_amount,
                entry_price,
                record
                    .prices
                    .as_ref()
                    .and_then(|p| p.current_mark_price)
                    .unwrap_or(entry_price),
                record.entry_processed_ms.unwrap_or_else(now_ms),
            );
            state.buy_lamports = record
                .buy_lamports
                .as_deref()
                .and_then(|v| v.parse().ok())
                .or(Some(slot.buy_amount_lamports));
            state.actual_entry_sol_amount = fill_sol;
            state.buy_signature = buy_sig;
            state.pool_tape_needed = Some(true);
            if let Err(err) = self.subscribe_pool(&descriptor).await {
                tracing::warn!(error = %err, mint = %descriptor.mint, "[07 POSITION] Recovered position but pool subscribe failed");
                continue;
            }
            {
                let mut registered = self.registered.lock().await;
                let entry = registered
                    .entry(key.clone())
                    .or_insert_with(|| (mint, HashSet::new()));
                entry.0 = mint;
                entry.1.insert(slot.name.as_str());
            }
            slot.tokens
                .lock()
                .await
                .insert(key, Arc::new(Mutex::new(state)));
            tracing::warn!(strategy = strategy_name.as_str(), mint = %descriptor.mint, token_amount, "[07 POSITION] Recovered existing position");
        }
        Ok(())
    }

    pub async fn close(&self) {
        tracing::info!("[SHUTDOWN] Trading runtime stopping");
        let mints: Vec<Pubkey> = self
            .registered
            .lock()
            .await
            .drain()
            .filter_map(|(_, (mint, _))| {
                if mint == Pubkey::default() {
                    None
                } else {
                    Some(mint)
                }
            })
            .collect();
        for mint in mints {
            let _ = self
                .geyser
                .commands
                .send(GeyserCommand::Unregister { mint })
                .await;
        }
    }
}

fn pump_alt_mint(
    descriptor: &crate::events::PoolDescriptor,
    event: &PoolTradeEvent,
) -> Option<Pubkey> {
    if !alt_venue(&descriptor.venue) || sol_pair(descriptor, event) {
        return None;
    }
    descriptor
        .quote_mint
        .as_deref()
        .or_else(|| {
            event
                .curve
                .as_ref()
                .and_then(|curve| curve.quote_mint.as_deref())
        })
        .and_then(|quote| quote.parse().ok())
}

fn alt_venue(venue: &str) -> bool {
    venue == "pump" || venue == "launchlab"
}

fn pair_quote_mint<'a>(
    descriptor: &'a crate::events::PoolDescriptor,
    event: &'a PoolTradeEvent,
) -> Option<&'a str> {
    descriptor
        .quote_mint
        .as_deref()
        .or_else(|| {
            event
                .curve
                .as_ref()
                .and_then(|curve| curve.quote_mint.as_deref())
        })
        .or_else(|| event.swap.as_ref().map(|swap| swap.quote_mint.as_str()))
}

fn sol_pair(descriptor: &crate::events::PoolDescriptor, event: &PoolTradeEvent) -> bool {
    crate::venues::ids::is_sol_quote(pair_quote_mint(descriptor, event))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
