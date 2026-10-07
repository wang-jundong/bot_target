/**
 * strategy_v_031 — sliding sell-hit inside a time gate.
 *
 * Faithful port of scalpingbot Strategy. One instance per mint.
 * After the target's buy, the buy is the last `buy_hit_count` sells in a row
 * above `dust_sol`, summing to at least `buy_hit_min`. That window has to finish
 * at least `min_entry_s` after this cycle's target buy, and no later than
 * `max_entry_s` (0 disables that cap). A window that is too early slides.
 * A window past the clock ends the cycle.
 * Dust prints are skipped and do not break the run. The other side does.
 * A short window slides: the oldest print drops, and the next print is checked
 * with the ones still in the run.
 * The exit is one sliding window of `sell_hit_count` buys summing to `sell_hit_min`,
 * a take-profit versus the fill, `max_hold_s` after the fill, or the target's max sell.
 * A max sell clears the tokens the target still holds (at least 80% of that bag).
 * `first_cycle_only` never starts another cycle after that max sell.
 * `repeat_entries` goes back to looking for a buy on the same clock after a flat
 * exit or a failed buy. The first max sell still ends that search.
 * Prints from our own wallet are left out of both runs.
 * A target buy at or before the max sell does not open a cycle.
 */

import type { StrategyV031Config } from "../config/strategyV031.js";

export const STRATEGY_NAME = "strategy_v_031";

const PHASE_IDLE = 0;
const PHASE_SEEK_SELL = 1;
const PHASE_PENDING = 2;
const PHASE_HOLD = 3;
const PHASE_WAIT = 4;
const PHASE_DONE = 5;

const SIDE_BUY = 1;
const SIDE_SELL = 2;

/** A target sell is the max sell when it takes at least this share of the tokens still held. */
const MAX_SELL_FRAC = 0.80;

const PHASE_NAMES = ["idle", "seek_sell", "pending", "hold", "wait", "done"] as const;
/** After a failed sell, wait before resubmitting so a quiet market is not hammered every tick. */
const SELL_RETRY_MS = 2_000;

export type StrategyV031Phase = (typeof PHASE_NAMES)[number];

export interface StrategyV031MarketEvent {
  signature: string;
  slot: number;
  transactionIndex?: number;
  eventIndex?: number;
  timestampSec: number;
  timestampMs: number;
  side: "BUY" | "SELL";
  wallet: string;
  solAmount: number;
  /** Raw token size of this print. Used to tell a max target sell from a partial. */
  tokenAmount: number;
  price: number;
}

/** Chain position. A missing transaction index sorts after any known index in the same slot. */
interface ChainCursor {
  slot: number;
  transactionIndex: number;
  eventIndex: number;
}

interface FlowState {
  phase: number;
  streakSide: number;
  prints: number[];
  why: string;
  hitSol: number;
  exitSol: number;
  /** Signal price that fired the buy, then the confirmed fill once that lands. */
  entryPx: number;
  heldSinceMs: number;
  targetTokens: number;
  /** Cycle clock. The target buy that opened this search. */
  t0Ms: number;
  px0: number;
  nowMs: number;
  diag: Record<string, number>;
  /** Last target buy that opened or added to the bag. */
  openedAt: ChainCursor | null;
  /** Last max target sell. A buy at or before this cursor does not open a cycle. */
  closedAt: ChainCursor | null;
}

function freshState(): FlowState {
  return {
    phase: PHASE_SEEK_SELL,
    streakSide: 0,
    prints: [],
    why: "",
    hitSol: 0,
    exitSol: 0,
    entryPx: 0,
    heldSinceMs: 0,
    targetTokens: 0,
    t0Ms: 0,
    px0: 0,
    nowMs: 0,
    diag: {},
    openedAt: null,
    closedAt: null
  };
}

function cursorOf(event: StrategyV031MarketEvent): ChainCursor {
  return {
    slot: event.slot,
    transactionIndex: event.transactionIndex ?? Number.MAX_SAFE_INTEGER,
    eventIndex: event.eventIndex ?? 0
  };
}

function isStrictlyAfter(next: ChainCursor, prev: ChainCursor): boolean {
  if (next.slot !== prev.slot) return next.slot > prev.slot;
  if (next.transactionIndex !== prev.transactionIndex) return next.transactionIndex > prev.transactionIndex;
  return next.eventIndex > prev.eventIndex;
}

function isMaxSell(held: number, sold: number): boolean {
  if (sold <= 0) return false;
  if (held <= 0) return true;
  return sold >= held * MAX_SELL_FRAC;
}

function clearStreak(st: FlowState): void {
  st.streakSide = 0;
  st.prints = [];
}

export class StrategyV031Engine {
  private readonly cfg: StrategyV031Config;
  private readonly ownWallet: string;
  private st: FlowState = freshState();
  private targetWallet = "";
  private bound = false;
  private buyReason = "";
  private sellReason = "";
  private buyDiag: Record<string, number> = {};
  private lastPx = 0;
  /** Set when a live sell failed and the bag is still open. The next clock resubmits that exit. */
  private exitRetry = false;
  private exitRetryAtMs = 0;

  constructor(cfg: StrategyV031Config, ownWallet = "") {
    this.cfg = cfg;
    this.ownWallet = ownWallet;
    this.st.phase = PHASE_IDLE;
  }

  get phaseName(): StrategyV031Phase {
    return PHASE_NAMES[this.st.phase] ?? "idle";
  }

  /** Other wallets' prints matter while a sell-run, a pending buy, or an exit is in progress. */
  needsPoolTape(): boolean {
    return this.exitRetry
      || this.st.phase === PHASE_SEEK_SELL
      || this.st.phase === PHASE_PENDING
      || this.st.phase === PHASE_HOLD;
  }

  get isBound(): boolean {
    return this.bound;
  }

  get isDone(): boolean {
    return this.st.phase === PHASE_DONE;
  }

  get lastBuyReason(): string {
    return this.buyReason || STRATEGY_NAME;
  }

  get lastSellReason(): string {
    return this.sellReason || "exit";
  }

  get lastBuyDiag(): Record<string, number> {
    return { ...this.buyDiag };
  }

  buySizeSol(): number {
    return this.cfg.size_sol;
  }

  /**
   * Gate first buy. That buy is not replayed, so the bag and the clock start here.
   * The tape starts on the following print, already seeking sells.
   */
  bindGate(wallet: string, gateTokenAmount = 0, price = 0, nowMs = 0): void {
    this.targetWallet = wallet;
    this.st = freshState();
    this.st.targetTokens = Math.max(gateTokenAmount, 0);
    this.st.t0Ms = Math.trunc(nowMs);
    this.st.nowMs = this.st.t0Ms;
    this.st.px0 = price > 0 ? price : 0;
    this.lastPx = this.st.px0;
    this.bound = true;
    this.buyReason = "";
    this.sellReason = "";
    this.buyDiag = {};
    this.exitRetry = false;
  }

  /** `fillPx` is the confirmed buy, in strategy SOL/token. It replaces the sell print as the take-profit baseline. */
  onBuyFill(fillPx = 0, heldSinceMs = this.st.nowMs): void {
    if (fillPx > 0) this.st.entryPx = fillPx;
    this.commitBuy(true);
    this.st.heldSinceMs = Math.trunc(heldSinceMs);
  }

  onBuyFailed(): void {
    this.exitRetry = false;
    if (this.st.phase === PHASE_DONE || this.st.phase === PHASE_IDLE) return;
    this.commitBuy(false);
    this.st.heldSinceMs = 0;
    this.resumeSeek();
  }

  /**
   * The signal already moved a strategy exit out of HOLD.
   * A live exit that did not come from this engine is still HOLD.
   */
  onSellFill(): void {
    this.exitRetry = false;
    this.st.heldSinceMs = 0;
    if (this.st.phase === PHASE_HOLD) {
      this.st.phase = PHASE_WAIT;
      this.st.why = "external_exit";
      clearStreak(this.st);
    }
    this.resumeSeek();
  }

  /**
   * The sell transaction failed and the tokens are still held.
   * The signal already left HOLD, so put the exit back and resubmit once the cooldown passes.
   */
  onSellFailed(nowMs: number): void {
    this.exitRetry = true;
    this.exitRetryAtMs = nowMs + SELL_RETRY_MS;
    this.st.phase = PHASE_HOLD;
    clearStreak(this.st);
  }

  /**
   * Resubmit a failed exit once `nowMs` is past the cooldown.
   * Otherwise sell a live hold that has hit the take-profit or the hold cap since the last print.
   */
  onClock(nowMs: number, markPx = 0): "SELL" | null {
    if (this.exitRetry) {
      if (nowMs < this.exitRetryAtMs) return null;
      this.exitRetry = false;
      this.st.phase = PHASE_WAIT;
      clearStreak(this.st);
      return "SELL";
    }
    if (markPx > 0) this.lastPx = markPx;
    if (this.st.phase !== PHASE_HOLD) return null;
    const px = markPx > 0 ? markPx : this.lastPx;
    const signal = this.riskExit(px, nowMs);
    if (signal) this.noteSignal(signal);
    return signal;
  }

  /** Target's max sell landed while the buy was still pending. Apply that exit once the fill is HOLD. */
  applyMissedTargetSell(): void {
    if (this.st.phase !== PHASE_HOLD) return;
    if (!this.cfg.target_sell_exit && !this.cfg.first_cycle_only) return;
    this.st.why = "target_sell";
    this.sellReason = "target_sell";
    this.afterMaxSell();
  }

  /** Rehydrate a recovered position. Entry price is the fill, in strategy SOL/token units. */
  restoreHold(entryPx: number, heldSinceMs = Date.now()): void {
    this.bound = true;
    this.st.phase = PHASE_HOLD;
    this.st.entryPx = entryPx;
    this.st.heldSinceMs = Math.trunc(heldSinceMs);
    this.st.why = "restored";
    clearStreak(this.st);
  }

  onEvent(event: StrategyV031MarketEvent): "BUY" | "SELL" | null {
    if (!this.bound) return null;
    if (this.exitRetry) return null;
    if (event.price > 0) this.lastPx = event.price;
    const side = event.side === "BUY" ? SIDE_BUY : event.side === "SELL" ? SIDE_SELL : 0;
    const isTarget = Boolean(this.targetWallet) && event.wallet === this.targetWallet && (side === SIDE_BUY || side === SIDE_SELL);
    const isOwn = Boolean(this.ownWallet) && event.wallet === this.ownWallet;
    return this.noteSignal(this.flowStep(side, event.solAmount, event.tokenAmount, event.price, isTarget, isOwn, event.timestampMs, cursorOf(event)));
  }

  private noteSignal(signal: "BUY" | "SELL" | null): "BUY" | "SELL" | null {
    if (signal === "BUY") {
      this.buyReason = `buy_hit ${this.st.hitSol.toFixed(2)} SOL`;
      this.buyDiag = { ...this.st.diag };
      return "BUY";
    }
    if (signal === "SELL") {
      if (this.st.why === "sell_hit") this.sellReason = `sell_hit ${this.st.exitSol.toFixed(2)} SOL`;
      else if (this.st.why === "take_profit") this.sellReason = "take_profit";
      else if (this.st.why === "hold_time") this.sellReason = "hold_time";
      else this.sellReason = "target_sell";
      return "SELL";
    }
    return null;
  }

  private commitBuy(ok: boolean): void {
    clearStreak(this.st);
    this.st.phase = ok ? PHASE_HOLD : PHASE_WAIT;
  }

  private ageS(nowMs: number): number {
    if (this.st.t0Ms <= 0) return 0;
    return Math.max(0, nowMs - this.st.t0Ms) / 1000;
  }

  private heldS(nowMs: number): number {
    if (this.st.heldSinceMs <= 0) return 0;
    return Math.max(0, nowMs - this.st.heldSinceMs) / 1000;
  }

  private expire(): void {
    this.st.phase = PHASE_WAIT;
    clearStreak(this.st);
  }

  /** The bag's max sell. `first_cycle_only` never starts another cycle. */
  private afterMaxSell(): void {
    clearStreak(this.st);
    this.st.phase = this.cfg.first_cycle_only ? PHASE_DONE : PHASE_IDLE;
  }

  /** After a flat exit, look for another buy on the same cycle clock. */
  private resumeSeek(): void {
    const st = this.st;
    const p = this.cfg;
    if (!p.repeat_entries || st.phase !== PHASE_WAIT) return;
    if (p.max_entry_s > 0 && this.ageS(st.nowMs) > p.max_entry_s) return;
    st.phase = PHASE_SEEK_SELL;
    st.entryPx = 0;
    st.hitSol = 0;
    clearStreak(st);
  }

  private flowStep(
    side: number,
    sol: number,
    tokens: number,
    px: number,
    isTarget: boolean,
    isOwn: boolean,
    nowMs: number,
    order: ChainCursor
  ): "BUY" | "SELL" | null {
    const st = this.st;
    const p = this.cfg;
    st.nowMs = nowMs;
    // A buy already closed by a max sell must not reopen the cycle. The sell is not applied again.
    if (isTarget && side === SIDE_BUY && st.closedAt && !isStrictlyAfter(order, st.closedAt)) return null;
    // A sell from before the current bag, or from an exit already seen, is not a second exit.
    if (isTarget && side === SIDE_SELL && this.sellAlreadyPassed(order)) return null;

    if (isTarget && side === SIDE_BUY) {
      st.targetTokens += Math.max(tokens, 0);
      st.openedAt = order;
    }

    if (isTarget && side === SIDE_SELL) {
      const maxSell = isMaxSell(st.targetTokens, tokens);
      st.targetTokens = Math.max(0, st.targetTokens - Math.max(tokens, 0));
      if (maxSell && st.phase !== PHASE_DONE) {
        if (!st.closedAt || isStrictlyAfter(order, st.closedAt)) st.closedAt = order;
        if (st.phase === PHASE_HOLD && (p.target_sell_exit || p.first_cycle_only)) {
          st.why = "target_sell";
          this.afterMaxSell();
          return "SELL";
        }
        if (st.phase === PHASE_SEEK_SELL || st.phase === PHASE_PENDING || st.phase === PHASE_WAIT) {
          this.afterMaxSell();
          return null;
        }
      }
    }

    if (isTarget && side === SIDE_BUY && st.phase === PHASE_IDLE) {
      st.phase = PHASE_SEEK_SELL;
      st.hitSol = 0;
      st.t0Ms = nowMs;
      st.px0 = px;
      clearStreak(st);
      return null;
    }

    if (st.phase === PHASE_SEEK_SELL && p.max_entry_s > 0 && this.ageS(nowMs) > p.max_entry_s) {
      this.expire();
      return null;
    }

    // Our own fill is not a market print. It does not extend or break either run.
    if (isOwn) return this.riskExit(px);

    if (sol <= p.dust_sol || (side !== SIDE_BUY && side !== SIDE_SELL)) {
      return this.riskExit(px);
    }
    if (st.phase !== PHASE_SEEK_SELL && st.phase !== PHASE_HOLD) return null;

    if (st.streakSide !== side) {
      st.streakSide = side;
      st.prints = [];
    }
    st.prints.push(sol);
    const rawNeed = st.phase === PHASE_SEEK_SELL && side === SIDE_SELL ? p.buy_hit_count : p.sell_hit_count;
    const need = rawNeed < 1 ? 1 : rawNeed;
    if (st.prints.length > need) st.prints.splice(0, st.prints.length - need);
    if (st.prints.length < need) return this.riskExit(px);

    const total = st.prints.reduce((sum, value) => sum + value, 0);
    const done = st.streakSide;
    if (st.phase === PHASE_SEEK_SELL && done === SIDE_SELL && total >= p.buy_hit_min) {
      const age = this.ageS(nowMs);
      if (p.min_entry_s > 0 && age < p.min_entry_s) {
        st.prints.shift();
        return null;
      }
      const mcap = px > 0 ? px * 1_000_000_000 : 0;
      const chase = st.px0 > 0 && px > 0 ? px / st.px0 - 1 : 0;
      st.hitSol = total;
      st.phase = PHASE_PENDING;
      st.why = "buy_hit";
      st.entryPx = px;
      st.diag = {
        wall_s: round(age, 3),
        end_ret: round(chase, 4),
        mcap: round(mcap, 3),
        buy_hit_sol: round(total, 4)
      };
      clearStreak(st);
      return "BUY";
    }
    if (st.phase === PHASE_HOLD && done === SIDE_BUY && total >= p.sell_hit_min) {
      st.phase = PHASE_WAIT;
      st.why = "sell_hit";
      st.exitSol = total;
      clearStreak(st);
      return "SELL";
    }
    // Sum is short. Drop the oldest print so the next one can complete a new window.
    st.prints.shift();
    return this.riskExit(px);
  }

  private sellAlreadyPassed(order: ChainCursor): boolean {
    const st = this.st;
    if (st.openedAt && isStrictlyAfter(st.openedAt, order)) return true;
    if (st.closedAt && isStrictlyAfter(st.closedAt, order)) return true;
    return false;
  }

  private riskExit(px: number, nowMs = this.st.nowMs): "SELL" | null {
    const st = this.st;
    const p = this.cfg;
    if (st.phase !== PHASE_HOLD) return null;
    if (p.take_profit > 0 && st.entryPx > 0 && px > 0 && px / st.entryPx - 1 >= p.take_profit) {
      st.phase = PHASE_WAIT;
      st.why = "take_profit";
      clearStreak(st);
      return "SELL";
    }
    if (p.max_hold_s > 0 && this.heldS(nowMs) >= p.max_hold_s) {
      st.phase = PHASE_WAIT;
      st.why = "hold_time";
      clearStreak(st);
      return "SELL";
    }
    return null;
  }
}

function round(x: number, ndigits: number): number {
  const f = 10 ** ndigits;
  return Math.round(x * f + Number.EPSILON) / f;
}
