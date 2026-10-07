import { describe, expect, it } from "vitest";
import { loadStrategyV031Config, type StrategyV031Config } from "../src/config/strategyV031.js";
import { StrategyV031Engine, type StrategyV031MarketEvent } from "../src/strategy/strategyV031Engine.js";

const T0 = 1_700_000_000_000;

const cfg = (overrides: Partial<StrategyV031Config> = {}): StrategyV031Config => ({
  ...loadStrategyV031Config(),
  ...overrides
});

const print = (overrides: Partial<StrategyV031MarketEvent> = {}): StrategyV031MarketEvent => ({
  signature: "sig",
  slot: 1,
  timestampSec: Math.floor(T0 / 1000),
  timestampMs: T0 + 5_000,
  side: "SELL",
  wallet: "other",
  solAmount: 0.3,
  tokenAmount: 0,
  price: 50e-9,
  ...overrides
});

function bound(overrides: Partial<StrategyV031Config> = {}, ownWallet = "", tokens = 1_000): StrategyV031Engine {
  const engine = new StrategyV031Engine(cfg(overrides), ownWallet);
  engine.bindGate("target", tokens, 50e-9, T0);
  return engine;
}

describe("strategy_v_031 engine", () => {
  it("loads the scalping knobs", () => {
    const loaded = loadStrategyV031Config();
    expect(loaded.gate_wallet).toBe("3VUNtVtjjx5ckUojT7UocJ5fbuAJRsNUXNfTBnPte9vC");
    expect(loaded.min_entry_s).toBe(4);
    expect(loaded.max_entry_s).toBe(20);
    expect(loaded.buy_hit_count).toBe(4);
    expect(loaded.buy_hit_min).toBe(1);
    expect(loaded.sell_hit_count).toBe(4);
    expect(loaded.sell_hit_min).toBe(4);
    expect(loaded.take_profit).toBe(0.25);
    expect(loaded.max_hold_s).toBe(85);
    expect(loaded.first_cycle_only).toBe(true);
    expect(loaded.repeat_entries).toBe(false);
    expect(loaded.size_sol).toBe(0.4);
  });

  it("treats a zero hit count as a window of one", () => {
    const engine = bound({ buy_hit_count: 0, buy_hit_min: 0.2, min_entry_s: 0 });
    expect(engine.onEvent(print({ solAmount: 0.3, timestampMs: T0 + 1_000, signature: "one" }))).toBe("BUY");
  });

  it("buys the sliding sell window once the clock is open", () => {
    const engine = bound();
    expect(engine.phaseName).toBe("seek_sell");
    expect(engine.needsPoolTape()).toBe(true);
    expect(sells(engine, "early", T0 + 1_000)).toBeNull();
    expect(engine.phaseName).toBe("seek_sell");
    const decision = engine.onEvent(print({ timestampMs: T0 + 5_000, signature: "open" }));
    expect(decision).toBe("BUY");
    expect(engine.phaseName).toBe("pending");
    expect(engine.lastBuyReason).toContain("buy_hit");
    expect(engine.lastBuyDiag.buy_hit_sol).toBe(1.2);
    expect(engine.lastBuyDiag.wall_s).toBe(5);
    expect(engine.lastBuyDiag.end_ret).toBe(0);
    expect(engine.lastBuyDiag.mcap).toBe(50);
  });

  it("slides a short sell window until the sum qualifies", () => {
    const engine = bound();
    expect(sells(engine, "short", T0 + 5_000, 0.2)).toBeNull();
    expect(engine.phaseName).toBe("seek_sell");
    const decision = engine.onEvent(print({ solAmount: 0.4, timestampMs: T0 + 6_000, signature: "slide" }));
    expect(decision).toBe("BUY");
    expect(engine.lastBuyDiag.buy_hit_sol).toBe(1);
  });

  it("ignores dust and resets the run when the other side prints", () => {
    const engine = bound();
    engine.onEvent(print({ timestampMs: T0 + 5_000, signature: "s1" }));
    engine.onEvent(print({ timestampMs: T0 + 5_000, signature: "s2" }));
    expect(engine.onEvent(print({ solAmount: 0.1, timestampMs: T0 + 5_000, signature: "dust" }))).toBeNull();
    engine.onEvent(print({ side: "BUY", solAmount: 1, timestampMs: T0 + 5_000, signature: "break" }));
    expect(sells(engine, "after", T0 + 6_000)).toBe("BUY");
  });

  it("ends the cycle when the window finishes after the clock", () => {
    const engine = bound();
    expect(sells(engine, "late", T0 + 20_001)).toBeNull();
    expect(engine.phaseName).toBe("wait");
    expect(engine.needsPoolTape()).toBe(false);
    expect(sells(engine, "after", T0 + 25_000)).toBeNull();
    expect(engine.phaseName).toBe("wait");

    const edge = bound();
    expect(sells(edge, "edge", T0 + 20_000)).toBe("BUY");
  });

  it("sells a hold on a sliding buy window, take-profit, hold time, or the target max sell", () => {
    const cluster = bound({ take_profit: 0 });
    arm(cluster);
    cluster.onBuyFill(50e-9, T0 + 5_000);
    expect(cluster.phaseName).toBe("hold");
    expect(buys(cluster, "short", T0 + 6_000, 0.5)).toBeNull();
    const sold = engineBuy(cluster, 2.5, T0 + 7_000, "slide");
    expect(sold).toBe("SELL");
    expect(cluster.lastSellReason).toContain("sell_hit");
    expect(cluster.phaseName).toBe("wait");

    const tp = bound();
    arm(tp);
    const fill = 50e-9;
    tp.onBuyFill(fill, T0 + 5_000);
    expect(tp.onEvent(print({ solAmount: 0.05, price: fill * 1.2, timestampMs: T0 + 6_000, signature: "under" }))).toBeNull();
    const took = tp.onEvent(print({ solAmount: 0.05, price: fill * 1.25, timestampMs: T0 + 6_000, signature: "tp" }));
    expect(took).toBe("SELL");
    expect(tp.lastSellReason).toBe("take_profit");

    const held = bound({ take_profit: 0 });
    arm(held);
    held.onBuyFill(fill, T0 + 5_000);
    expect(held.onClock(T0 + 5_000 + 84_999, fill)).toBeNull();
    expect(held.onClock(T0 + 5_000 + 85_000, fill)).toBe("SELL");
    expect(held.lastSellReason).toBe("hold_time");

    const target = bound({ take_profit: 0 });
    arm(target);
    target.onBuyFill(fill, T0 + 5_000);
    expect(target.onEvent(print({ side: "SELL", wallet: "target", solAmount: 0.05, tokenAmount: 100, timestampMs: T0 + 6_000, signature: "partial" }))).toBeNull();
    expect(target.phaseName).toBe("hold");
    const dumped = target.onEvent(print({ side: "SELL", wallet: "target", solAmount: 0.05, tokenAmount: 900, timestampMs: T0 + 7_000, signature: "max" }));
    expect(dumped).toBe("SELL");
    expect(target.lastSellReason).toBe("target_sell");
    expect(target.phaseName).toBe("done");
  });

  it("does not start another cycle after the first max sell", () => {
    const engine = bound();
    expect(engine.onEvent(print({ side: "SELL", wallet: "target", solAmount: 1, tokenAmount: 1_000, timestampMs: T0 + 1_000, signature: "flat" }))).toBeNull();
    expect(engine.phaseName).toBe("done");
    expect(engine.needsPoolTape()).toBe(false);
    expect(engine.onEvent(print({ side: "BUY", wallet: "target", solAmount: 1, tokenAmount: 1_000, slot: 3, timestampMs: T0 + 30_000, signature: "again" }))).toBeNull();
    expect(engine.phaseName).toBe("done");
    expect(sells(engine, "later", T0 + 35_000)).toBeNull();
  });

  it("looks for another buy on the same clock when repeat entries is on", () => {
    const engine = bound({ repeat_entries: true, take_profit: 0.25 });
    arm(engine);
    engine.onBuyFill(50e-9, T0 + 5_000);
    expect(engine.onEvent(print({ solAmount: 0.05, price: 50e-9 * 1.25, timestampMs: T0 + 8_000, signature: "tp" }))).toBe("SELL");
    expect(engine.phaseName).toBe("wait");
    engine.onSellFill();
    expect(engine.phaseName).toBe("seek_sell");
    expect(sells(engine, "again", T0 + 10_000)).toBe("BUY");

    const missed = bound({ repeat_entries: true });
    arm(missed);
    missed.onBuyFill(50e-9, T0 + 5_000);
    expect(missed.onEvent(print({ solAmount: 0.05, price: 50e-9 * 1.25, timestampMs: T0 + 21_000, signature: "late-tp" }))).toBe("SELL");
    missed.onSellFill();
    expect(missed.phaseName).toBe("wait");
  });

  it("retries a failed sell and keeps a failed buy from reopening a closed bag", () => {
    const engine = bound({ take_profit: 0 });
    arm(engine);
    engine.onBuyFill(50e-9, T0 + 5_000);
    expect(buys(engine, "exit", T0 + 6_000, 1)).toBe("SELL");
    engine.onSellFailed(T0 + 6_000);
    expect(engine.phaseName).toBe("hold");
    expect(engine.onClock(T0 + 7_000)).toBeNull();
    expect(engine.onClock(T0 + 8_000)).toBe("SELL");
    expect(engine.lastSellReason).toContain("sell_hit");

    const failed = bound();
    arm(failed);
    expect(failed.onEvent(print({ side: "SELL", wallet: "target", solAmount: 1, tokenAmount: 1_000, timestampMs: T0 + 6_000, signature: "during" }))).toBeNull();
    expect(failed.phaseName).toBe("done");
    failed.onBuyFailed();
    expect(failed.phaseName).toBe("done");
  });

  it("sells a fill that landed after the target already max-sold", () => {
    const engine = bound();
    arm(engine);
    expect(engine.onEvent(print({ side: "SELL", wallet: "target", solAmount: 1, tokenAmount: 1_000, timestampMs: T0 + 6_000, signature: "during" }))).toBeNull();
    expect(engine.phaseName).toBe("done");
    engine.onBuyFill(50e-9, T0 + 6_000);
    expect(engine.phaseName).toBe("hold");
    engine.applyMissedTargetSell();
    expect(engine.phaseName).toBe("done");
    expect(engine.lastSellReason).toBe("target_sell");
  });

  it("leaves our own prints out of both runs", () => {
    const engine = bound({}, "me");
    engine.onEvent(print({ timestampMs: T0 + 5_000, signature: "s1" }));
    expect(engine.onEvent(print({ side: "BUY", wallet: "me", solAmount: 3, timestampMs: T0 + 5_000, signature: "my-buy" }))).toBeNull();
    engine.onEvent(print({ timestampMs: T0 + 5_000, signature: "s2" }));
    expect(engine.onEvent(print({ wallet: "me", solAmount: 2, timestampMs: T0 + 5_000, signature: "my-sell" }))).toBeNull();
    engine.onEvent(print({ timestampMs: T0 + 5_000, signature: "s3" }));
    expect(engine.onEvent(print({ timestampMs: T0 + 5_000, signature: "s4" }))).toBe("BUY");
    engine.onBuyFill(50e-9, T0 + 5_000);
    engine.onEvent(print({ side: "BUY", solAmount: 1, timestampMs: T0 + 6_000, signature: "b1" }));
    expect(engine.onEvent(print({ side: "BUY", wallet: "me", solAmount: 5, timestampMs: T0 + 6_000, signature: "my-exit" }))).toBeNull();
    expect(buys(engine, "rest", T0 + 7_000, 1, 3)).toBe("SELL");
  });

  it("does not reopen a cycle when the target buy arrives before the max sell", () => {
    const engine = bound({ first_cycle_only: false });
    expect(engine.onEvent(print({ side: "SELL", wallet: "target", solAmount: 1, tokenAmount: 1_000, slot: 20, transactionIndex: 5, timestampMs: T0 + 2_000, signature: "flat" }))).toBeNull();
    expect(engine.phaseName).toBe("idle");
    expect(engine.onEvent(print({ side: "BUY", wallet: "target", solAmount: 1, tokenAmount: 1_000, slot: 19, timestampMs: T0 + 1_000, signature: "late" }))).toBeNull();
    expect(engine.phaseName).toBe("idle");
    expect(engine.onEvent(print({ side: "BUY", wallet: "target", solAmount: 1, tokenAmount: 1_000, slot: 21, timestampMs: T0 + 30_000, price: 60e-9, signature: "next" }))).toBeNull();
    expect(engine.phaseName).toBe("seek_sell");
    expect(sells(engine, "reborn", T0 + 35_000)).toBe("BUY");
    expect(engine.lastBuyDiag.mcap).toBe(50);
    expect(engine.lastBuyDiag.end_ret).toBe(-0.1667);
  });
});

function sells(engine: StrategyV031Engine, tag: string, timestampMs: number, solAmount = 0.3): "BUY" | "SELL" | null {
  let last: "BUY" | "SELL" | null = null;
  for (let i = 0; i < 4; i++) last = engine.onEvent(print({ solAmount, timestampMs, signature: `${tag}-${i}` }));
  return last;
}

function buys(engine: StrategyV031Engine, tag: string, timestampMs: number, solAmount: number, count = 4): "BUY" | "SELL" | null {
  let last: "BUY" | "SELL" | null = null;
  for (let i = 0; i < count; i++) last = engineBuy(engine, solAmount, timestampMs, `${tag}-${i}`);
  return last;
}

function engineBuy(engine: StrategyV031Engine, solAmount: number, timestampMs: number, signature: string): "BUY" | "SELL" | null {
  return engine.onEvent(print({ side: "BUY", solAmount, timestampMs, signature }));
}

function arm(engine: StrategyV031Engine): void {
  const decision = sells(engine, "arm", T0 + 5_000);
  if (decision !== "BUY") throw new Error("arm did not reach a buy");
}
