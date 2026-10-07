import { describe, expect, it } from "vitest";
import { loadStrategyV011Config } from "../src/config/strategyV011.js";
import { PHASE_DONE, PHASE_HOLDING, PHASE_WATCHING, StrategyV011Engine } from "../src/strategy/strategyV011Engine.js";
import type { StrategyMarketEvent } from "../src/strategy/strategyV011Engine.js";

const cfg = () => loadStrategyV011Config();

const T0 = 1_700_000_000_000;

const event = (overrides: Partial<StrategyMarketEvent> = {}): StrategyMarketEvent => ({
  signature: "sig",
  slot: 1,
  timestampSec: 1_700_000_000,
  timestampMs: T0,
  side: "BUY",
  wallet: "other",
  solAmount: 0.8,
  tokenAmount: 1_000,
  price: 50e-9,
  ...overrides
});

function bind(engine: StrategyV011Engine, tokens = 1_000_000, nowMs = T0) {
  return engine.bindGateBuy({
    price: 50e-9,
    sol: 3,
    tsSec: 1_700_000_000,
    wallet: "target",
    gateSig: "gate",
    tokenAmount: tokens,
    nowMs
  });
}

function enter(engine: StrategyV011Engine, nowMs = T0) {
  bind(engine, 1_000_000, nowMs);
  const minMs = cfg().min_entry_s * 1000;
  return engine.onTimer(50e-9, nowMs + minMs + 500);
}

describe("strategy_v_011 engine", () => {
  it("binds a gate buy into watching without a size clip", () => {
    const engine = new StrategyV011Engine(cfg());
    const decision = bind(engine, 1_000_000);
    expect(decision.kind).toBe("none");
    expect(engine.phaseName).toBe(PHASE_WATCHING);
    const small = new StrategyV011Engine(cfg());
    expect(small.bindGateBuy({
      price: 50e-9,
      sol: 0.2,
      tsSec: 1_700_000_000,
      wallet: "target",
      gateSig: "gate",
      tokenAmount: 100,
      nowMs: T0
    }).kind).toBe("none");
    expect(small.phaseName).toBe(PHASE_WATCHING);
  });

  it("fires after the watch window when price has not chased", () => {
    const engine = new StrategyV011Engine(cfg());
    const config = cfg();
    bind(engine);
    expect(engine.onTimer(50e-9, T0 + (config.min_entry_s - 0.5) * 1000).kind).toBe("none");
    const decision = engine.onTimer(50e-9, T0 + (config.min_entry_s + 0.5) * 1000);
    expect(decision.kind).toBe("fire_buy");
    if (decision.kind === "fire_buy") expect(decision.reason).toContain("entry");
  });

  it("waits out a chase and aborts once the entry window expires", () => {
    const engine = new StrategyV011Engine(cfg());
    const config = cfg();
    bind(engine);
    const chased = 50e-9 * (1 + config.max_chase + 0.01);
    const held = engine.onTimer(chased, T0 + (config.min_entry_s + 0.5) * 1000);
    expect(held.kind).toBe("none");
    expect(engine.phaseName).toBe(PHASE_WATCHING);
    const expired = engine.onTimer(chased, T0 + (config.max_entry_s + 1) * 1000);
    expect(expired.kind).toBe("skip");
    if (expired.kind === "skip") expect(expired.detail).toContain("entry_expired");
    expect(engine.phaseName).toBe(PHASE_DONE);
  });

  it("takes profit against the fill", () => {
    const engine = new StrategyV011Engine(cfg());
    const config = cfg();
    expect(enter(engine).kind).toBe("fire_buy");
    engine.onBuyFill(50e-9, 1_700_000_006, 10);
    expect(engine.phaseName).toBe(PHASE_HOLDING);
    const flat = engine.onTimer(50e-9 * (1 + config.take_profit * 0.5), T0 + 8_000);
    expect(flat.kind).toBe("none");
    const decision = engine.onTimer(50e-9 * (1 + config.take_profit + 0.01), T0 + 9_000);
    expect(decision.kind).toBe("fire_sell");
    if (decision.kind === "fire_sell") expect(decision.reason).toContain("take_profit");
  });

  it("retries an exit after the sell fails", () => {
    const engine = new StrategyV011Engine(cfg());
    const config = cfg();
    expect(enter(engine).kind).toBe("fire_buy");
    engine.onBuyFill(50e-9, 1_700_000_006, 10);
    const px = 50e-9 * (1 + config.take_profit + 0.01);
    const first = engine.onTimer(px, T0 + 8_000);
    expect(first.kind).toBe("fire_sell");
    engine.onSellFailed();
    expect(engine.onTimer(px, T0 + 8_000).kind).toBe("none");
    const retry = engine.onTimer(px, T0 + 10_000);
    expect(retry.kind).toBe("fire_sell");
    expect(engine.phaseName).toBe(PHASE_HOLDING);
  });

  it("exits on a target max sell and ignores a partial", () => {
    const engine = new StrategyV011Engine(cfg());
    expect(enter(engine).kind).toBe("fire_buy");
    engine.onBuyFill(50e-9, 1_700_000_006, 10);
    const partial = engine.onEvent(event({
      side: "SELL",
      wallet: "target",
      signature: "partial",
      tokenAmount: 100_000,
      solAmount: 0.4,
      timestampMs: T0 + 7_000
    }), true);
    expect(partial.kind).toBe("none");
    const maxSell = engine.onEvent(event({
      side: "SELL",
      wallet: "target",
      signature: "max",
      tokenAmount: 900_000,
      solAmount: 2.5,
      timestampMs: T0 + 8_000
    }), true);
    expect(maxSell.kind).toBe("fire_sell");
    if (maxSell.kind === "fire_sell") expect(maxSell.reason).toContain("target_sell");
  });

  it("exits when the last buys sum past the sell-hit minimum", () => {
    const engine = new StrategyV011Engine(cfg());
    const config = cfg();
    expect(enter(engine).kind).toBe("fire_buy");
    engine.onBuyFill(50e-9, 1_700_000_006, 10);
    const each = config.sell_hit_min / config.sell_hit_count + 0.05;
    let decision: ReturnType<StrategyV011Engine["onEvent"]> = { kind: "none" };
    for (let i = 0; i < config.sell_hit_count - 1; i++) {
      decision = engine.onEvent(event({
        signature: `buy-${i}`,
        solAmount: each,
        timestampMs: T0 + 7_000 + i
      }), true);
      expect(decision.kind).toBe("none");
    }
    decision = engine.onEvent(event({
      signature: "buy-last",
      solAmount: each,
      timestampMs: T0 + 8_000
    }), true);
    expect(decision.kind).toBe("fire_sell");
    if (decision.kind === "fire_sell") expect(decision.reason).toContain("sell_hit");
  });

  it("drops the buy streak when a sell prints, and ignores dust", () => {
    const engine = new StrategyV011Engine(cfg());
    const config = cfg();
    expect(enter(engine).kind).toBe("fire_buy");
    engine.onBuyFill(50e-9, 1_700_000_006, 10);
    const each = config.sell_hit_min / config.sell_hit_count + 0.1;
    for (let i = 0; i < config.sell_hit_count - 1; i++) {
      engine.onEvent(event({ signature: `buy-${i}`, solAmount: each, timestampMs: T0 + 7_000 + i }), true);
    }
    engine.onEvent(event({
      side: "SELL",
      wallet: "other",
      signature: "break",
      solAmount: 0.5,
      timestampMs: T0 + 7_500
    }), true);
    for (let i = 0; i < config.sell_hit_count; i++) {
      const decision = engine.onEvent(event({
        signature: `dust-${i}`,
        solAmount: config.dust_sol,
        timestampMs: T0 + 7_600 + i
      }), true);
      expect(decision.kind).toBe("none");
    }
    let last: ReturnType<StrategyV011Engine["onEvent"]> = { kind: "none" };
    for (let i = 0; i < config.sell_hit_count - 1; i++) {
      last = engine.onEvent(event({ signature: `again-${i}`, solAmount: each, timestampMs: T0 + 8_000 + i }), true);
      expect(last.kind).toBe("none");
    }
    last = engine.onEvent(event({ signature: "again-last", solAmount: each, timestampMs: T0 + 9_000 }), true);
    expect(last.kind).toBe("fire_sell");
  });

  it("aborts the cycle when the target max-sells before our buy, then rearms on the next buy", () => {
    const engine = new StrategyV011Engine(cfg());
    const config = cfg();
    bind(engine, 1_000);
    const sold = engine.onEvent(event({
      side: "SELL",
      wallet: "target",
      signature: "dump",
      tokenAmount: 1_000,
      solAmount: 3,
      timestampMs: T0 + 1_000
    }), false);
    expect(sold.kind).toBe("skip");
    if (sold.kind === "skip") expect(sold.detail).toContain("sold_max");
    expect(engine.phaseName).toBe(PHASE_DONE);

    const rearmAt = T0 + 30_000;
    const rearm = engine.onEvent(event({
      side: "BUY",
      wallet: "target",
      signature: "rebuy",
      tokenAmount: 2_000,
      solAmount: 4,
      price: 60e-9,
      timestampMs: rearmAt
    }), false);
    expect(rearm.kind).toBe("none");
    expect(engine.phaseName).toBe(PHASE_WATCHING);
    expect(engine.onTimer(60e-9, rearmAt + (config.min_entry_s - 0.5) * 1000).kind).toBe("none");
    const decision = engine.onTimer(60e-9, rearmAt + (config.min_entry_s + 0.5) * 1000);
    expect(decision.kind).toBe("fire_buy");
  });

  it("sells after the fill when the target max-sold while the buy was in flight", () => {
    const engine = new StrategyV011Engine(cfg());
    expect(enter(engine).kind).toBe("fire_buy");
    const sold = engine.onEvent(event({
      side: "SELL",
      wallet: "target",
      signature: "dump",
      tokenAmount: 1_000_000,
      solAmount: 3,
      timestampMs: T0 + 6_000
    }), false);
    expect(sold.kind).toBe("none");
    expect(engine.phaseName).toBe(PHASE_WATCHING);
    const reason = engine.onBuyFill(50e-9, 1_700_000_006, 10);
    expect(reason).toContain("target_sell");
    expect(engine.phaseName).toBe(PHASE_HOLDING);
    expect(engine.lastSellReason).toContain("target_sell");
  });
});
