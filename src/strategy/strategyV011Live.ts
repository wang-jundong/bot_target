import type { Keypair } from "@solana/web3.js";
import type { Logger } from "pino";
import type { StrategyV011Config } from "../config/strategyV011.js";
import type { PoolTradeEvent } from "../events/types.js";
import type { TokenState } from "../state/tokenState.js";
import { TokenLifecycleState } from "./state.js";
import type { Strategy, StrategyExecution } from "./types.js";
import { StrategyV011Engine, type StrategyDecision, type StrategyMarketEvent } from "./strategyV011Engine.js";
import { lamportsToSol, solToLamportsNumber, toStrategyPrice } from "./priceUnits.js";

export class StrategyV011Live implements Strategy {
  readonly #engines = new WeakMap<TokenState, StrategyV011Engine>();
  readonly #busy = new WeakSet<TokenState>();
  readonly #sellAfterFill = new WeakSet<TokenState>();
  readonly #poolNeeded = new WeakMap<TokenState, boolean>();
  #poolTape?: (state: TokenState, needed: boolean, fromSlot?: number) => void;
  readonly timerMs: number;

  constructor(
    private readonly cfg: StrategyV011Config,
    private readonly execution: StrategyExecution,
    private readonly wallet: Keypair,
    private readonly targetWallet: string,
    private readonly defaultBuySlippageBps: number,
    private readonly defaultSellSlippageBps: number,
    private readonly canOpenPosition: (state: TokenState) => boolean,
    private readonly logger: Logger
  ) {
    this.timerMs = cfg.timer_ms > 0 ? cfg.timer_ms : 200;
  }

  setPoolTape(listener: (state: TokenState, needed: boolean, fromSlot?: number) => void): void {
    this.#poolTape = listener;
  }

  onEvent(state: TokenState, event: PoolTradeEvent): void {
    const engine = this.#engine(state);
    if (!engine) return;
    const market = toMarketEvent(event);
    const holding = isHolding(state);
    if (engine.phaseName === "unbound") {
      const bind = engine.bindGateBuy({
        price: market.price,
        sol: lamportsToSol(state.targetBuy.solAmount),
        tsSec: Math.floor(state.targetBuy.timestampMs / 1000),
        wallet: this.targetWallet,
        gateSig: state.targetBuy.signature,
        tokenAmount: Number(state.targetBuy.tokenAmount),
        nowMs: state.targetBuy.timestampMs
      });
      this.#logDecision(state, bind, "gate");
      this.#syncPool(state, engine, event.slot);
      if (bind.kind === "skip" || engine.isDone) return;
      // The gate buy starts the clock. Later pool events and timers decide the entry.
      if (event.signature === state.targetBuy.signature && event.eventIndex === state.targetBuy.eventIndex) return;
    }
    const decision = engine.onEvent(market, holding);
    this.#syncPool(state, engine, event.slot);
    void this.#dispatch(state, engine, decision);
  }

  onClock(state: TokenState, nowMs = Date.now()): void {
    const engine = this.#engine(state);
    if (!engine) return;
    this.#syncPool(state, engine);
    if (engine.isDone || !engine.timerWantsTicks()) return;
    const mark = toStrategyPrice(state.prices.currentMarkPrice ?? 0) || engine.lastMarkPx;
    const decision = engine.onTimer(mark, nowMs);
    this.#syncPool(state, engine);
    void this.#dispatch(state, engine, decision);
  }

  onBuyFill(state: TokenState, fill: { price: number; slot: number }): void {
    const engine = this.#engines.get(state);
    if (!engine) return;
    const missed = engine.onBuyFill(toStrategyPrice(fill.price), Math.floor(Date.now() / 1000), fill.slot);
    this.#syncPool(state, engine, fill.slot);
    if (missed) this.#sellAfterFill.add(state);
  }

  onBuyFailed(state: TokenState): { rearm: boolean } {
    this.#sellAfterFill.delete(state);
    const engine = this.#engines.get(state);
    if (!engine) return { rearm: true };
    engine.onBuyFailed();
    state.resetBuySendClaim();
    this.#syncPool(state, engine);
    return { rearm: true };
  }

  onSellFailed(state: TokenState): void {
    this.#engines.get(state)?.onSellFailed(Date.now());
    const engine = this.#engines.get(state);
    if (engine) this.#syncPool(state, engine);
  }

  onSellFill(state: TokenState): { rearm: boolean } {
    this.#sellAfterFill.delete(state);
    const engine = this.#engines.get(state);
    if (!engine) return { rearm: true };
    engine.onSellFill();
    this.#syncPool(state, engine);
    return { rearm: true };
  }

  /** Rehydrate an already-open recovered position into HOLDING. */
  restoreOpenPosition(state: TokenState, fillPriceLive: number): void {
    const engine = this.#engine(state);
    if (!engine) return;
    if (engine.phaseName === "unbound") {
      const sol = lamportsToSol(state.targetBuy.solAmount);
      engine.bindGateBuy({
        price: toStrategyPrice(state.targetBuy.price || fillPriceLive),
        sol,
        tsSec: Math.floor((state.targetBuy.timestampMs || Date.now()) / 1000),
        wallet: this.targetWallet,
        gateSig: state.targetBuy.signature,
        tokenAmount: Number(state.targetBuy.tokenAmount),
        nowMs: state.targetBuy.timestampMs || Date.now()
      });
    }
    engine.onBuyFill(toStrategyPrice(fillPriceLive), Math.floor(Date.now() / 1000), 0);
    this.#syncPool(state, engine);
  }

  #syncPool(state: TokenState, engine: StrategyV011Engine, fromSlot?: number): void {
    const needed = engine.needsPoolTape();
    if (this.#poolNeeded.get(state) === needed) return;
    this.#poolNeeded.set(state, needed);
    this.#poolTape?.(state, needed, needed ? fromSlot : undefined);
  }

  #engine(state: TokenState): StrategyV011Engine | undefined {
    let engine = this.#engines.get(state);
    if (!engine) {
      engine = new StrategyV011Engine(this.cfg);
      this.#engines.set(state, engine);
    }
    return engine;
  }

  async #dispatch(state: TokenState, engine: StrategyV011Engine, decision: StrategyDecision): Promise<void> {
    this.#logDecision(state, decision, "signal");
    if (decision.kind === "none" || decision.kind === "skip") return;
    if (this.#busy.has(state)) return;
    if (decision.kind === "fire_buy") {
      await this.#buy(state, engine, decision.reason);
      return;
    }
    await this.#sell(state, decision.reason);
  }

  async #buy(state: TokenState, engine: StrategyV011Engine, reason: string): Promise<void> {
    if (this.#busy.has(state) || isHolding(state)) return;
    if (!state.claimBuySend()) return;
    this.#busy.add(state);
    let sent = false;
    try {
      if (!this.canOpenPosition(state)) {
        this.#sellAfterFill.delete(state);
        state.resetBuySendClaim();
        engine.onBuyFailed();
        this.execution.recordEntryEvent(state, "entry_blocked", { reason });
        return;
      }
      const sizeSol = engine.buySizeSol();
      state.buyLamports = solToLamportsNumber(sizeSol);
      state.prices.entrySignalPrice = state.prices.currentMarkPrice || state.targetBuy.price;
      state.buySlippageBps = this.defaultBuySlippageBps;
      state.sellSlippageBps = this.defaultSellSlippageBps;
      this.logger.info({ mint: state.descriptor.mint, reason, sizeSol, diag: engine.lastBuyDiag }, "[05 ENTRY] strategy_v_011 buy signal");
      state.preparedBuy = await state.adapter.buildBuy({
        descriptor: state.descriptor,
        owner: this.wallet.publicKey,
        lamports: state.buyLamports,
        slippageBps: state.buySlippageBps
      });
      state.transition(TokenLifecycleState.BUY_PREPARED);
      sent = true;
      await this.execution.sendBuy(state, performance.now());
    } catch (error) {
      this.logger.error({ err: error instanceof Error ? error.message : String(error), mint: state.descriptor.mint }, "[05 ENTRY] strategy_v_011 buy failed");
      this.#sellAfterFill.delete(state);
      engine.onBuyFailed();
      state.resetBuySendClaim();
      try {
        if (state.lifecycle === TokenLifecycleState.BUY_PREPARED) state.transition(TokenLifecycleState.TRACKING_POOL);
      } catch { /* ignore */ }
    } finally {
      this.#busy.delete(state);
    }
    // sendBuy calls onBuyFill while #busy is set, and #sell bails in that window.
    if (sent && this.#sellAfterFill.delete(state) && isHolding(state)) {
      await this.#sell(state, engine.lastSellReason);
    }
  }

  async #sell(state: TokenState, reason: string): Promise<void> {
    if (this.#busy.has(state) || !isHolding(state)) return;
    if (!state.claimSellSend()) return;
    this.#busy.add(state);
    try {
      this.logger.info({ mint: state.descriptor.mint, reason }, "[08 EXIT] strategy_v_011 sell signal");
      state.prices.exitSignalPrice = state.prices.currentMarkPrice;
      await this.execution.sendSell(state, reason, performance.now());
    } catch (error) {
      this.logger.error({ err: error instanceof Error ? error.message : String(error), mint: state.descriptor.mint }, "[08 EXIT] strategy_v_011 sell failed");
      state.releaseSellSend();
      this.onSellFailed(state);
    } finally {
      this.#busy.delete(state);
    }
  }

  #logDecision(state: TokenState, decision: StrategyDecision, stage: string): void {
    if (decision.kind === "none") return;
    this.logger.info({ mint: state.descriptor.mint, stage, decision }, "[04 STRAT] strategy_v_011 decision");
  }
}

function isHolding(state: TokenState): boolean {
  return state.lifecycle === TokenLifecycleState.POSITION_ACTIVE_UNCONFIRMED
    || state.lifecycle === TokenLifecycleState.POSITION_ACTIVE_CONFIRMED
    || state.lifecycle === TokenLifecycleState.BUY_CONFIRMED
    || state.lifecycle === TokenLifecycleState.SELL_PREPARED
    || state.lifecycle === TokenLifecycleState.BUY_PROCESSED;
}

function toMarketEvent(event: PoolTradeEvent): StrategyMarketEvent {
  return {
    signature: event.signature,
    slot: event.slot,
    timestampSec: Math.floor(event.timestampMs / 1000),
    timestampMs: event.timestampMs,
    side: event.side === "buy" ? "BUY" : "SELL",
    wallet: event.trader,
    solAmount: lamportsToSol(event.solAmount),
    tokenAmount: Number(event.tokenAmount),
    price: toStrategyPrice(event.price)
  };
}
