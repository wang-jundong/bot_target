/**
 * strategy_v_022 — round-window sell-hit, then a buy-run exit.
 *
 * Faithful port of scalpingbot Strategy. One instance per mint.
 * After bind (the target's first buy), a finished round is `buy_hit_count` sells
 * in a row above `dust_sol`. Qualifying rounds before `buy_hit_round` are skipped.
 * The buy is that round when it sums to at least `buy_hit_min`, unless market cap
 * is above `max_mc_sol`. A short round is dropped and the next sells start a new one.
 * Dust prints are skipped and do not break the run. The other side does.
 * The exit is one round of `sell_hit_count` buys summing to `sell_hit_min`,
 * a take-profit, a loss cut, or the target's max sell.
 * A max sell clears the tokens the target still holds (at least 80% of that bag).
 * That exit is kept by chain order, including one that arrives while idle.
 * The next target buy opens a round only when it lands strictly after that sell.
 * A smaller partial sell does not, and it still counts as a market print.
 * Prints from our own wallet are left out of both runs.
 * A setup that never qualifies waits for that max sell, then the next target buy.
 */

import type { StrategyV022Config } from "../config/strategyV022.js";

export const STRATEGY_NAME = "strategy_v_022";

const PHASE_IDLE = 0;
const PHASE_SEEK_SELL = 1;
const PHASE_PENDING = 2;
const PHASE_HOLD = 3;
const PHASE_WAIT = 4;

const SIDE_BUY = 1;
const SIDE_SELL = 2;

/** A target sell is the max sell when it takes at least this share of the tokens still held. */
const MAX_SELL_FRAC = 0.80;

const PHASE_NAMES = ["idle", "seek_sell", "pending", "hold", "wait"] as const;
/** After a failed sell, wait before resubmitting so a quiet market is not hammered every tick. */
const SELL_RETRY_MS = 2_000;

export type StrategyV022Phase = (typeof PHASE_NAMES)[number];

export interface StrategyV022MarketEvent {
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
  /** Qualifying sell rounds seen in the current seek. The buy is round `buy_hit_round`. */
  hits: number;
  hitSol: number;
  exitSol: number;
  /** Sell-print price that fired the buy, then the confirmed fill once that lands. */
  entryPx: number;
  targetTokens: number;
  /** Last target buy that opened or added to the bag. */
  openedAt: ChainCursor | null;
  /** Last max target sell. A buy at or before this cursor does not open a round. */
  closedAt: ChainCursor | null;
}

function freshState(): FlowState {
  return {
    phase: PHASE_SEEK_SELL,
    streakSide: 0,
    prints: [],
    why: "",
    hits: 0,
    hitSol: 0,
    exitSol: 0,
    entryPx: 0,
    targetTokens: 0,
    openedAt: null,
    closedAt: null
  };
}

function cursorOf(event: StrategyV022MarketEvent): ChainCursor {
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

export class StrategyV022Engine {
  private readonly cfg: StrategyV022Config;
  private readonly ownWallet: string;
  private st: FlowState = freshState();
  private targetWallet = "";
  private bound = false;
  private buyReason = "";
  private sellReason = "";
  private buyDiag: Record<string, unknown> = {};
  /** Set when a live sell failed and the bag is still open. The next clock resubmits that exit. */
  private exitRetry = false;
  private exitRetryAtMs = 0;

  constructor(cfg: StrategyV022Config, ownWallet = "") {
    this.cfg = cfg;
    this.ownWallet = ownWallet;
    this.st.phase = PHASE_IDLE;
  }

  get phaseName(): StrategyV022Phase {
    return PHASE_NAMES[this.st.phase] ?? "idle";
  }

  /** Other wallets' prints matter only while a sell-run or an exit is in progress. */
  needsPoolTape(): boolean {
    return this.exitRetry || this.st.phase === PHASE_SEEK_SELL || this.st.phase === PHASE_PENDING || this.st.phase === PHASE_HOLD;
  }

  get isBound(): boolean {
    return this.bound;
  }

  get lastBuyReason(): string {
    return this.buyReason || STRATEGY_NAME;
  }

  get lastSellReason(): string {
    return this.sellReason || "exit";
  }

  get lastBuyDiag(): Record<string, unknown> {
    return { ...this.buyDiag };
  }

  buySizeSol(): number {
    return this.cfg.size_sol;
  }

  /**
   * Gate first buy. That buy is not replayed, so the bag starts at its token size.
   * The tape starts on the following print, already seeking sells.
   */
  bindGate(wallet: string, gateTokenAmount = 0): void {
    this.targetWallet = wallet;
    this.st = freshState();
    this.st.targetTokens = Math.max(gateTokenAmount, 0);
    this.bound = true;
    this.buyReason = "";
    this.sellReason = "";
    this.buyDiag = {};
  }

  /** `fillPx` is the confirmed buy, in strategy SOL/token. It replaces the sell print as the take-profit baseline. */
  onBuyFill(fillPx = 0): void {
    if (fillPx > 0) this.st.entryPx = fillPx;
    this.commitBuy(true);
  }

  onBuyFailed(): void {
    this.commitBuy(false);
  }

  /**
   * Python `on_sell_fill` is a no-op because the signal already moved the phase.
   * A live exit that did not come from this engine is still HOLD.
   */
  onSellFill(): void {
    this.exitRetry = false;
    if (this.st.phase !== PHASE_HOLD) return;
    this.st.phase = PHASE_WAIT;
    this.st.why = "external_exit";
    clearStreak(this.st);
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

  /** Resubmit a failed exit once `nowMs` is past the cooldown. No-op otherwise. */
  onClock(nowMs: number): "SELL" | null {
    if (!this.exitRetry || nowMs < this.exitRetryAtMs) return null;
    this.exitRetry = false;
    this.st.phase = PHASE_WAIT;
    clearStreak(this.st);
    return "SELL";
  }

  /** Target's max sell landed while the buy was still pending. Apply that exit once the fill is HOLD. */
  applyMissedTargetSell(): void {
    if (this.st.phase !== PHASE_HOLD || !this.cfg.target_sell_exit) return;
    this.st.phase = PHASE_IDLE;
    this.st.why = "target_sell";
    this.sellReason = "target_sell";
    clearStreak(this.st);
  }

  /** Rehydrate a recovered position. Entry price is the fill, in strategy SOL/token units. */
  restoreHold(entryPx: number): void {
    this.bound = true;
    this.st.phase = PHASE_HOLD;
    this.st.entryPx = entryPx;
    this.st.why = "restored";
    clearStreak(this.st);
  }

  onEvent(event: StrategyV022MarketEvent): "BUY" | "SELL" | null {
    if (!this.bound) return null;
    if (this.exitRetry) return null;
    const side = event.side === "BUY" ? SIDE_BUY : event.side === "SELL" ? SIDE_SELL : 0;
    const isTarget = Boolean(this.targetWallet) && event.wallet === this.targetWallet && (side === SIDE_BUY || side === SIDE_SELL);
    const isOwn = Boolean(this.ownWallet) && event.wallet === this.ownWallet;
    const signal = this.flowStep(side, event.solAmount, event.tokenAmount, event.price, isTarget, isOwn, cursorOf(event));
    if (signal === "BUY") {
      this.buyReason = `buy_hit ${this.st.hitSol.toFixed(2)} SOL`;
      this.buyDiag = { buy_hit_sol: round(this.st.hitSol, 4) };
      return "BUY";
    }
    if (signal === "SELL") {
      if (this.st.why === "sell_hit") this.sellReason = `sell_hit ${this.st.exitSol.toFixed(2)} SOL`;
      else if (this.st.why === "stop") this.sellReason = "stop";
      else if (this.st.why === "take_profit") this.sellReason = "take_profit";
      else this.sellReason = "target_sell";
      return "SELL";
    }
    return null;
  }

  private commitBuy(ok: boolean): void {
    clearStreak(this.st);
    this.st.phase = ok ? PHASE_HOLD : PHASE_WAIT;
  }

  private flowStep(side: number, sol: number, tokens: number, px: number, isTarget: boolean, isOwn: boolean, order: ChainCursor): "BUY" | "SELL" | null {
    const st = this.st;
    const p = this.cfg;
    // A buy already closed by a max sell must not reopen the round. The sell is not applied again.
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
      if (maxSell) {
        if (!st.closedAt || isStrictlyAfter(order, st.closedAt)) st.closedAt = order;
        if (st.phase === PHASE_HOLD && p.target_sell_exit) {
          st.phase = PHASE_IDLE;
          st.why = "target_sell";
          clearStreak(st);
          return "SELL";
        }
        if (st.phase === PHASE_SEEK_SELL || st.phase === PHASE_PENDING) {
          st.phase = PHASE_IDLE;
          st.hits = 0;
          clearStreak(st);
          return null;
        }
        if (st.phase === PHASE_WAIT) {
          st.phase = PHASE_IDLE;
          return null;
        }
      }
    }

    if (isTarget && side === SIDE_BUY && st.phase === PHASE_IDLE) {
      st.phase = PHASE_SEEK_SELL;
      st.hitSol = 0;
      st.hits = 0;
      clearStreak(st);
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
    if (st.prints.length < need) return this.riskExit(px);

    const total = st.prints.reduce((sum, value) => sum + value, 0);
    const done = st.streakSide;
    if (st.phase === PHASE_SEEK_SELL && done === SIDE_SELL && total >= p.buy_hit_min) {
      st.hits += 1;
      st.prints = [];
      if (st.hits < p.buy_hit_round) return this.riskExit(px);
      const mcap = px > 0 ? px * 1_000_000_000 : 0;
      if (p.max_mc_sol > 0 && mcap > p.max_mc_sol) {
        st.phase = PHASE_WAIT;
        st.hits = 0;
        clearStreak(st);
        return null;
      }
      st.hitSol = total;
      st.phase = PHASE_PENDING;
      st.why = "buy_hit";
      st.entryPx = px;
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
    // Sum is short, or this side is not the one this phase is scoring.
    // The round is finished. The next prints start a new one.
    st.prints = [];
    return this.riskExit(px);
  }

  private sellAlreadyPassed(order: ChainCursor): boolean {
    const st = this.st;
    if (st.openedAt && isStrictlyAfter(st.openedAt, order)) return true;
    if (st.closedAt && isStrictlyAfter(st.closedAt, order)) return true;
    return false;
  }

  private riskExit(px: number): "SELL" | null {
    const st = this.st;
    const p = this.cfg;
    if (st.phase !== PHASE_HOLD) return null;
    if (p.take_profit > 0 && st.entryPx > 0 && px > 0 && px / st.entryPx - 1 >= p.take_profit) {
      st.phase = PHASE_WAIT;
      st.why = "take_profit";
      clearStreak(st);
      return "SELL";
    }
    if (p.stop_loss > 0 && st.entryPx > 0 && px > 0 && px / st.entryPx - 1 <= -p.stop_loss) {
      st.phase = PHASE_WAIT;
      st.why = "stop";
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
