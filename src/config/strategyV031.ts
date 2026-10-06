export interface StrategyV031Config {
  gate_wallet: string;
  /** Prints at or below this size are ignored and do not break a run. */
  dust_sol: number;
  /** The sell window must finish at least this many seconds after the cycle's target buy. 0 disables it. */
  min_entry_s: number;
  /** Abort the entry search once the target buy is older than this. 0 disables it. */
  max_entry_s: number;
  /** Consecutive sells, above dust, that can trigger a buy. */
  buy_hit_count: number;
  /** Those sells must sum to at least this many SOL. */
  buy_hit_min: number;
  /** Consecutive buys, above dust, that can trigger an exit. */
  sell_hit_count: number;
  /** Those buys must sum to at least this many SOL. */
  sell_hit_min: number;
  /** Take profit versus the fill. 0 disables it. */
  take_profit: number;
  /** Sell a position still open this many seconds after the fill. 0 disables it. */
  max_hold_s: number;
  /** Exit when the target sells at least 95% of the tokens still held. */
  target_sell_exit: boolean;
  /** After this bag's first max sell, a later target buy does not start another cycle. */
  first_cycle_only: boolean;
  /** After a flat exit or a failed buy, look for another entry on the same cycle clock. */
  repeat_entries: boolean;
  /** Live position size in SOL. */
  size_sol: number;
}

/**
 * Edit strategy knobs here (replaces scalpingbot config/strategy_v_031.json).
 * Buy is the last `buy_hit_count` sells in a row summing to at least `buy_hit_min`.
 * That window must finish between `min_entry_s` and `max_entry_s` after this cycle's
 * target buy. A short window slides: the oldest print drops and the next one is
 * checked with the rest. Sell is a sliding buy window, a take-profit versus the
 * fill, `max_hold_s` after the fill, or the target's max sell.
 */
export const STRATEGY_V_031_CONFIG: StrategyV031Config = {
  gate_wallet: "3VUNtVtjjx5ckUojT7UocJ5fbuAJRsNUXNfTBnPte9vC",
  dust_sol: 0.1,
  min_entry_s: 4.0,
  max_entry_s: 20.0,
  buy_hit_count: 4,
  buy_hit_min: 1.0,
  sell_hit_count: 4,
  sell_hit_min: 4.0,
  take_profit: 0.25,
  max_hold_s: 85.0,
  target_sell_exit: true,
  first_cycle_only: true,
  repeat_entries: false,
  size_sol: 0.03
};

export function loadStrategyV031Config(): StrategyV031Config {
  const cfg = { ...STRATEGY_V_031_CONFIG };
  if (cfg.buy_hit_count < 1) cfg.buy_hit_count = 1;
  if (cfg.sell_hit_count < 1) cfg.sell_hit_count = 1;
  return cfg;
}
