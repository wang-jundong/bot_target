import type { Keypair } from "@solana/web3.js";
import type { Logger } from "pino";
import type { StrategyV031Config } from "../config/strategyV031.js";
import type { PoolTradeEvent } from "../events/types.js";
import type { TokenState } from "../state/tokenState.js";
import { TokenLifecycleState } from "./state.js";
import type { Strategy, StrategyExecution } from "./types.js";
import { StrategyV031Engine, type StrategyV031MarketEvent } from "./strategyV031Engine.js";
import { lamportsToSol, solToLamportsNumber, toStrategyPrice } from "./priceUnits.js";

export class StrategyV031Live implements Strategy {
  readonly #engines = new WeakMap<TokenState, StrategyV031Engine>();
  readonly #busy = new WeakSet<TokenState>();
  readonly #sellAfterFill = new WeakSet<TokenState>();
  readonly #poolNeeded = new WeakMap<TokenState, boolean>();
  #poolTape?: (state: TokenState, needed: boolean, fromSlot?: number) => void;
  readonly timerMs = 1_000;

  constructor(
    private readonly cfg: StrategyV031Config,
    private readonly execution: StrategyExecution,
    private readonly wallet: Keypair,
    private readonly targetWallet: string,
    private readonly defaultBuySlippageBps: number,
    private readonly defaultSellSlippageBps: number,
    private readonly canOpenPosition: (state: TokenState) => boolean,
    private readonly logger: Logger
  ) {}

  setPoolTape(listener: (state: TokenState, needed: boolean, fromSlot?: number) => void): void {
    this.#poolTape = listener;
  }

  onEvent(state: TokenState, event: PoolTradeEvent): void {
    const engine = this.#engine(state);
    if (this.#isGate(state, event)) {
      if (!engine.isBound) {
        engine.bindGate(
          this.targetWallet,
          event.side === "buy" ? Number(event.tokenAmount) : 0,
          toStrategyPrice(event.price),
          event.timestampMs
        );
      }
      this.#syncPool(state, engine, event.slot);
      return;
    }
    if (!engine.isBound) {
      engine.bindGate(this.targetWallet, 0, toStrategyPrice(state.targetBuy.price), state.targetBuy.timestampMs);
    }
    const market = toMarketEvent(event);
    const before = engine.phaseName;
    const signal = engine.onEvent(market);
    const cycleEnded = engine.phaseName === "idle" || engine.phaseName === "done";
    if ((this.cfg.target_sell_exit || this.cfg.first_cycle_only) && before === "pending" && cycleEnded && market.side === "SELL" && market.wallet === this.targetWallet) {
      this.#sellAfterFill.add(state);
    }
    this.#syncPool(state, engine, event.slot);
    void this.#dispatch(state, engine, signal);
  }

  onClock(state: TokenState, nowMs = Date.now()): void {
    const engine = this.#engines.get(state);
    if (!engine || this.#busy.has(state) || !isHolding(state)) return;
    const mark = toStrategyPrice(state.prices.currentMarkPrice ?? 0);
    const signal = engine.onClock(nowMs, mark);
    this.#syncPool(state, engine);
    if (signal === "SELL") void this.#dispatch(state, engine, signal);
  }

  onBuyFill(state: TokenState, fill?: { price: number; slot: number }): void {
    const engine = this.#engines.get(state);
    if (!engine) return;
    engine.onBuyFill(toStrategyPrice(fill?.price ?? 0), Date.now());
    if (this.#sellAfterFill.has(state)) engine.applyMissedTargetSell();
    this.#syncPool(state, engine);
  }

  onBuyFailed(state: TokenState): { rearm: boolean } {
    this.#sellAfterFill.delete(state);
    const engine = this.#engines.get(state);
    engine?.onBuyFailed();
    if (engine) this.#syncPool(state, engine);
    return { rearm: true };
  }

  onSellFailed(state: TokenState): void {
    const engine = this.#engines.get(state);
    if (!engine) return;
    engine.onSellFailed(Date.now());
    this.#syncPool(state, engine);
  }

  onSellFill(state: TokenState): { rearm: boolean } {
    this.#sellAfterFill.delete(state);
    const engine = this.#engines.get(state);
    engine?.onSellFill();
    if (engine) this.#syncPool(state, engine);
    return { rearm: true };
  }

  restoreOpenPosition(state: TokenState, fillPriceLive: number): void {
    const engine = this.#engine(state);
    if (!engine.isBound) engine.bindGate(this.targetWallet, 0, toStrategyPrice(state.targetBuy.price), state.targetBuy.timestampMs || Date.now());
    engine.restoreHold(toStrategyPrice(fillPriceLive), Date.now());
    this.#syncPool(state, engine);
  }

  #syncPool(state: TokenState, engine: StrategyV031Engine, fromSlot?: number): void {
    const needed = engine.needsPoolTape();
    if (this.#poolNeeded.get(state) === needed) return;
    this.#poolNeeded.set(state, needed);
    this.#poolTape?.(state, needed, needed ? fromSlot : undefined);
  }

  #engine(state: TokenState): StrategyV031Engine {
    let engine = this.#engines.get(state);
    if (!engine) {
      engine = new StrategyV031Engine(this.cfg, this.wallet.publicKey.toBase58());
      this.#engines.set(state, engine);
    }
    return engine;
  }

  async #dispatch(state: TokenState, engine: StrategyV031Engine, signal: "BUY" | "SELL" | null): Promise<void> {
    if (signal === null) return;
    this.logger.info({ mint: state.descriptor.mint, signal, phase: engine.phaseName }, "[04 STRAT] strategy_v_031 decision");
    if (this.#busy.has(state)) return;
    if (signal === "BUY") {
      await this.#buy(state, engine, engine.lastBuyReason);
      return;
    }
    await this.#sell(state, engine.lastSellReason);
  }

  async #buy(state: TokenState, engine: StrategyV031Engine, reason: string): Promise<void> {
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
      this.logger.info({ mint: state.descriptor.mint, reason, sizeSol, diag: engine.lastBuyDiag }, "[05 ENTRY] strategy_v_031 buy signal");
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
      this.logger.error({ err: error instanceof Error ? error.message : String(error), mint: state.descriptor.mint }, "[05 ENTRY] strategy_v_031 buy failed");
      this.#sellAfterFill.delete(state);
      engine.onBuyFailed();
      state.resetBuySendClaim();
      try {
        if (state.lifecycle === TokenLifecycleState.BUY_PREPARED) state.transition(TokenLifecycleState.TRACKING_POOL);
      } catch { /* ignore */ }
    } finally {
      this.#busy.delete(state);
    }
    if (sent && this.#sellAfterFill.delete(state) && isHolding(state)) {
      await this.#sell(state, "target_sell");
    }
  }

  async #sell(state: TokenState, reason: string): Promise<void> {
    if (this.#busy.has(state) || !isHolding(state)) return;
    if (!state.claimSellSend()) return;
    this.#busy.add(state);
    try {
      this.logger.info({ mint: state.descriptor.mint, reason }, "[08 EXIT] strategy_v_031 sell signal");
      state.prices.exitSignalPrice = state.prices.currentMarkPrice;
      await this.execution.sendSell(state, reason, performance.now());
    } catch (error) {
      this.logger.error({ err: error instanceof Error ? error.message : String(error), mint: state.descriptor.mint }, "[08 EXIT] strategy_v_031 sell failed");
      state.releaseSellSend();
      this.onSellFailed(state);
    } finally {
      this.#busy.delete(state);
    }
  }

  #isGate(state: TokenState, event: PoolTradeEvent): boolean {
    return event.signature === state.targetBuy.signature && event.eventIndex === state.targetBuy.eventIndex;
  }
}

function isHolding(state: TokenState): boolean {
  return state.lifecycle === TokenLifecycleState.POSITION_ACTIVE_UNCONFIRMED
    || state.lifecycle === TokenLifecycleState.POSITION_ACTIVE_CONFIRMED
    || state.lifecycle === TokenLifecycleState.BUY_CONFIRMED
    || state.lifecycle === TokenLifecycleState.SELL_PREPARED
    || state.lifecycle === TokenLifecycleState.BUY_PROCESSED;
}

function toMarketEvent(event: PoolTradeEvent): StrategyV031MarketEvent {
  return {
    signature: event.signature,
    slot: event.slot,
    transactionIndex: event.transactionIndex,
    eventIndex: event.eventIndex,
    timestampSec: Math.floor(event.timestampMs / 1000),
    timestampMs: event.timestampMs,
    side: event.side === "buy" ? "BUY" : "SELL",
    wallet: event.trader,
    solAmount: lamportsToSol(event.solAmount),
    tokenAmount: Number(event.tokenAmount),
    price: toStrategyPrice(event.price)
  };
}
