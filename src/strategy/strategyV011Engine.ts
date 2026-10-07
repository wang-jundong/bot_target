/**
 * strategy_v_011 — wall-clock entry + mark/event exits.
 *
 * Faithful port of scalpingbot Strategy. One instance per mint.
 * Live uses wall clock; backtest replaces `_nowMs` from event timestamps / timer ticks.
 * A failed live sell is resubmitted once, after a short cooldown.
 */

import type { StrategyV011Config } from "../config/strategyV011.js";

export const STRATEGY_NAME = "strategy_v_011";

export const PHASE_UNBOUND = "unbound";
export const PHASE_WATCHING = "watching";
export const PHASE_HOLDING = "holding";
export const PHASE_DONE = "done";

/** A target sell is the max sell when it takes at least this share of the tokens still held. */
const MAX_SELL_FRAC = 0.80;
/** After a failed sell, wait before resubmitting so a still-true signal is not hammered every tick. */
const SELL_RETRY_MS = 2_000;

export type StrategyDecision =
  | { kind: "none" }
  | { kind: "skip"; detail: string }
  | { kind: "fire_buy"; reason: string }
  | { kind: "fire_sell"; reason: string };

export interface StrategyMarketEvent {
  signature: string;
  slot: number;
  timestampSec: number;
  timestampMs: number;
  side: "BUY" | "SELL";
  wallet: string;
  solAmount: number;
  /** Raw token size of this print. Used to tell a max target sell from a partial. */
  tokenAmount: number;
  price: number;
}

export class StrategyV011Engine {
  private readonly cfg: StrategyV011Config;
  private readonly _timerMs: number;

  private phase: string = PHASE_UNBOUND;
  private px0 = 0;
  private _t0WallMs = 0;
  private _nowMs = 0;
  private bound = false;
  private eligible = false;
  private entryDone = false;
  private aborted = false;
  private targetMaxSold = false;
  private targetTokens = 0;
  private gateBuySig = "";
  private _lastMarkPx = 0;
  private sellPrints: number[] = [];
  private entryPrice = 0;
  private peakPrice = 0;
  private exitInFlight = false;
  private exitRetry = false;
  private sellRetryAtMs = 0;
  private targetWallet = "";

  private _lastBuyReason = "";
  private _lastBuyDiag: Record<string, unknown> = {};
  private _lastSkip = "";
  private _lastSellReason = "";

  constructor(cfg: StrategyV011Config) {
    this.cfg = cfg;
    this._timerMs = cfg.timer_ms > 0 ? Math.trunc(cfg.timer_ms) : 200;
  }

  get timerMs(): number {
    return this._timerMs;
  }

  get phaseName(): string {
    return this.phase;
  }

  get isDone(): boolean {
    return this.phase === PHASE_DONE;
  }

  /** Other wallets' prints matter while watching for entry or holding a position. */
  needsPoolTape(): boolean {
    return this.phase === PHASE_WATCHING || this.phase === PHASE_HOLDING;
  }

  get lastMarkPx(): number {
    return this._lastMarkPx;
  }

  get lastSkip(): string {
    return this._lastSkip;
  }

  get lastBuyReason(): string {
    return this._lastBuyReason || STRATEGY_NAME;
  }

  get lastSellReason(): string {
    return this._lastSellReason || "exit signal";
  }

  get lastBuyDiag(): Record<string, unknown> {
    return { ...this._lastBuyDiag };
  }

  buySizeSol(): number {
    return Number(this.cfg.size_sol);
  }

  timerWantsTicks(): boolean {
    const watchingLive = this.phase === PHASE_WATCHING && this.eligible && !this.entryDone;
    const retryDue = this.exitRetry && this.phase === PHASE_HOLDING;
    return watchingLive || retryDue;
  }

  bindGateBuy(args: {
    price: number;
    sol: number;
    tsSec: number;
    wallet: string;
    gateSig: string;
    tokenAmount: number;
    nowMs: number;
  }): StrategyDecision {
    this._t0WallMs = Math.trunc(args.nowMs);
    this._nowMs = this._t0WallMs;
    return this.applyDecision(this.bindGateBuyCore(
      args.price,
      args.sol,
      args.wallet,
      args.gateSig,
      args.tokenAmount
    ));
  }

  onBuyFill(fillPrice: number, _fillTsSec: number, _fillSlot: number): void {
    this.entryPrice = fillPrice;
    this.peakPrice = fillPrice;
    this.phase = PHASE_HOLDING;
    this.exitInFlight = false;
    this.exitRetry = false;
    this.sellRetryAtMs = 0;
    this.sellPrints = [];
    if (fillPrice > 0) this.notePrice(fillPrice);
  }

  onBuyFailed(): void {
    this.entryPrice = 0;
    this.exitInFlight = false;
    this.exitRetry = false;
    this.phase = PHASE_DONE;
    this.entryDone = true;
  }

  onSellFailed(nowMs = this._nowMs): void {
    this.exitInFlight = false;
    this.exitRetry = true;
    this._nowMs = Math.trunc(nowMs);
    this.sellRetryAtMs = this._nowMs + SELL_RETRY_MS;
  }

  onSellFill(): void {
    this.entryPrice = 0;
    this.exitInFlight = false;
    this.exitRetry = false;
    this.phase = PHASE_DONE;
    this.entryDone = true;
  }

  onEvent(event: StrategyMarketEvent, holding: boolean): StrategyDecision {
    this._nowMs = Math.trunc(event.timestampMs);
    return this.applyDecision(this.onEventCore(event, holding));
  }

  onTimer(markPx: number, nowMs: number): StrategyDecision {
    this._nowMs = Math.trunc(nowMs);
    return this.applyDecision(this.onTimerCore(markPx));
  }

  private none(): StrategyDecision {
    return { kind: "none" };
  }

  private skip(detail: string): StrategyDecision {
    return { kind: "skip", detail };
  }

  private fireBuy(reason: string): StrategyDecision {
    return { kind: "fire_buy", reason };
  }

  private fireSell(reason: string): StrategyDecision {
    return { kind: "fire_sell", reason };
  }

  private applyDecision(decision: StrategyDecision): StrategyDecision {
    if (decision.kind === "fire_buy") this._lastBuyReason = decision.reason;
    else if (decision.kind === "fire_sell") this._lastSellReason = decision.reason;
    else if (decision.kind === "skip") this._lastSkip = decision.detail;
    return decision;
  }

  private bindGateBuyCore(price: number, sol: number, wallet: string, gateSig: string, tokens: number): StrategyDecision {
    if (this.bound) return this.none();
    this.targetWallet = wallet;
    this.px0 = price;
    this.bound = true;
    this.targetMaxSold = false;
    this.targetTokens = this.resolveTokens(tokens, price, sol);
    this.gateBuySig = gateSig;
    this.sellPrints = [];
    this._lastMarkPx = price > 0 ? price : 0;
    if (price > 0) this.notePrice(price);
    this.eligible = true;
    this.phase = PHASE_WATCHING;
    return this.none();
  }

  private resolveTokens(tokens: number, price: number, sol: number): number {
    const tok = Math.max(Number(tokens) || 0, 0);
    if (tok <= 0 && price > 0 && sol > 0) return sol / price;
    return tok;
  }

  private notePrice(px: number): void {
    if (px <= 0) return;
    this._lastMarkPx = px;
  }

  private wallAgeS(): number {
    return Math.max(0, this._nowMs - this._t0WallMs) / 1000;
  }

  private isMaxSell(held: number, sold: number): boolean {
    if (sold <= 0) return false;
    if (held <= 0) return true;
    return sold >= held * MAX_SELL_FRAC;
  }

  /** Update the target bag. Returns true when this print is a target max sell. */
  private noteTargetBag(event: StrategyMarketEvent): boolean {
    if (!this.targetWallet || event.wallet !== this.targetWallet) return false;
    const tokens = this.resolveTokens(event.tokenAmount, event.price, event.solAmount);
    if (event.side === "BUY") {
      if (this.gateBuySig && event.signature === this.gateBuySig) return false;
      this.targetTokens += tokens;
      return false;
    }
    if (event.side !== "SELL") return false;
    const maxSell = this.isMaxSell(this.targetTokens, tokens);
    if (maxSell) this.targetMaxSold = true;
    this.targetTokens = Math.max(0, this.targetTokens - tokens);
    return maxSell;
  }

  /** Sliding window of consecutive buys above dust. Returns a sell decision or null. */
  private noteSellHit(event: StrategyMarketEvent): StrategyDecision | null {
    const cfg = this.cfg;
    if (cfg.sell_hit_count <= 0 || cfg.sell_hit_min <= 0) return null;
    const sol = Number(event.solAmount) || 0;
    if (sol <= cfg.dust_sol) return null;
    if (event.side === "SELL") {
      this.sellPrints = [];
      return null;
    }
    if (event.side !== "BUY") return null;
    this.sellPrints.push(sol);
    const need = cfg.sell_hit_count;
    if (this.sellPrints.length > need) this.sellPrints = this.sellPrints.slice(-need);
    if (this.sellPrints.length < need) return null;
    const total = this.sellPrints.reduce((sum, value) => sum + value, 0);
    if (total >= cfg.sell_hit_min) {
      this.sellPrints = [];
      this.exitInFlight = true;
      return this.fireSell(`sell_hit — last ${need} buys sum=${total.toFixed(3)} SOL >= ${cfg.sell_hit_min.toFixed(3)}`);
    }
    this.sellPrints.shift();
    return null;
  }

  private abortSoldMax(): StrategyDecision | null {
    if (!this.targetMaxSold) return null;
    this.aborted = true;
    this.entryDone = true;
    this.phase = PHASE_DONE;
    return this.skip("sold_max — target max sell before our buy");
  }

  private rearmWatchFromBuy(event: StrategyMarketEvent): void {
    const price = Number(event.price) || 0;
    const sol = Number(event.solAmount) || 0;
    this.px0 = price;
    this._t0WallMs = event.timestampMs ? Math.trunc(event.timestampMs) : Math.trunc(event.timestampSec) * 1000;
    this._nowMs = this._t0WallMs;
    this.gateBuySig = event.signature;
    this.targetTokens = this.resolveTokens(event.tokenAmount, price, sol);
    this.targetMaxSold = false;
    this.entryDone = false;
    this.aborted = false;
    this.entryPrice = 0;
    this.peakPrice = 0;
    this.exitInFlight = false;
    this.exitRetry = false;
    this.sellPrints = [];
    this._lastMarkPx = price > 0 ? price : 0;
    if (price > 0) this.notePrice(price);
    this.eligible = true;
    this.phase = PHASE_WATCHING;
  }

  private maybeRearm(event: StrategyMarketEvent): boolean {
    if (!this.targetMaxSold) return false;
    if (event.side !== "BUY") return false;
    if (!this.targetWallet || event.wallet !== this.targetWallet) return false;
    if (this.gateBuySig && event.signature === this.gateBuySig) return false;
    this.rearmWatchFromBuy(event);
    return true;
  }

  private tryEntry(px: number, slot: number): StrategyDecision {
    const cfg = this.cfg;
    if (this.entryDone || this.aborted || this.phase === PHASE_DONE) {
      return this.skip("mint already attempted (entry_done) — no re-entry");
    }
    if (!this.eligible || this.phase !== PHASE_WATCHING) return this.skip("mint not eligible after gate bind");
    const aborted = this.abortSoldMax();
    if (aborted) return aborted;
    if (px <= 0) return this.none();

    const ageS = this.wallAgeS();
    const watch = cfg.min_entry_s > 0 && cfg.min_entry_s === cfg.min_entry_s ? cfg.min_entry_s : 0;
    const maxAge = cfg.max_entry_s;
    if (ageS < watch) return this.none();
    if (ageS > maxAge) {
      this.entryDone = true;
      this.aborted = true;
      this.phase = PHASE_DONE;
      return this.skip(`entry_expired — wall age=${ageS.toFixed(1)}s > max ${maxAge}s — abort`);
    }

    const endRet = this.px0 > 0 ? px / this.px0 - 1 : 0;
    if (cfg.max_chase > 0 && endRet >= cfg.max_chase) return this.none();

    this.entryDone = true;
    const mcap = px * 1_000_000_000;
    const diag = `wall=${ageS.toFixed(2)}s end_ret=${endRet.toFixed(3)} mcap=${mcap.toFixed(1)} slot=${slot}`;
    this._lastBuyDiag = {
      wall_s: round(ageS, 3),
      end_ret: round(endRet, 4),
      mcap: round(mcap, 3),
      slot
    };
    return this.fireBuy(`entry | ${diag}`);
  }

  private exitBlocked(): boolean {
    return this.exitInFlight || this.exitRetry;
  }

  private onHoldMark(px: number): StrategyDecision {
    const cfg = this.cfg;
    if (this.exitBlocked()) return this.none();
    if (px <= 0) return this.none();
    if (px > this.peakPrice) this.peakPrice = px;
    if (cfg.take_profit > 0 && this.entryPrice > 0) {
      const ret = px / this.entryPrice - 1;
      if (ret >= cfg.take_profit) {
        this.exitInFlight = true;
        return this.fireSell(`take_profit | ret=${ret.toFixed(3)} >= ${cfg.take_profit.toFixed(3)} (vs fill)`);
      }
    }
    return this.none();
  }

  private onHoldEvent(event: StrategyMarketEvent): StrategyDecision {
    const cfg = this.cfg;
    if (this.exitBlocked()) return this.none();
    const px = event.price;
    if (px > 0) this.notePrice(px);

    const maxSell = this.noteTargetBag(event);
    if (cfg.target_sell_exit && maxSell) {
      const ret = this.entryPrice > 0 && px > 0 ? px / this.entryPrice - 1 : 0;
      this.exitInFlight = true;
      return this.fireSell(`target_sell — gate max sell ${event.solAmount.toFixed(3)} SOL | ret=${ret.toFixed(3)}`);
    }

    const hit = this.noteSellHit(event);
    if (hit) return hit;
    return this.onHoldMark(px > 0 ? px : this._lastMarkPx);
  }

  private retryExit(): StrategyDecision | null {
    if (!this.exitRetry) return null;
    if (this._nowMs < this.sellRetryAtMs) return this.none();
    this.exitRetry = false;
    if (this.phase !== PHASE_HOLDING) return this.none();
    this.exitInFlight = true;
    return this.fireSell(this._lastSellReason || "exit retry");
  }

  private onTimerCore(markPx: number): StrategyDecision {
    const retry = this.retryExit();
    if (retry) return retry;
    if (!this.bound) return this.none();
    if (markPx > 0) this.notePrice(markPx);
    const px = markPx > 0 ? markPx : this._lastMarkPx;

    if (this.phase === PHASE_HOLDING || (this.entryPrice > 0 && !this.entryDone)) {
      if (this.phase === PHASE_HOLDING) return this.onHoldMark(px);
    }
    if (this.phase === PHASE_WATCHING) {
      if (this.entryDone && this.entryPrice <= 0) return this.none();
      return this.tryEntry(px, 0);
    }
    return this.none();
  }

  private onEventCore(event: StrategyMarketEvent, holding: boolean): StrategyDecision {
    if (!this.bound) return this.none();
    if (this.exitRetry) return this.none();
    if (holding || this.phase === PHASE_HOLDING) return this.onHoldEvent(event);
    if (this.maybeRearm(event)) return this.none();

    this.noteTargetBag(event);
    if (event.price > 0) this.notePrice(event.price);
    if (this.entryDone && !this.aborted && this.entryPrice <= 0) return this.none();
    if (this.phase === PHASE_WATCHING) {
      const aborted = this.abortSoldMax();
      if (aborted) return aborted;
      const px = event.price > 0 ? event.price : this._lastMarkPx;
      return this.tryEntry(px, event.slot);
    }
    return this.none();
  }
}

function round(x: number, ndigits: number): number {
  const f = 10 ** ndigits;
  return Math.round(x * f + Number.EPSILON) / f;
}
