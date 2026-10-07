export interface StrategyV011Config {
  gate_wallet: string;
  timer_ms: number;
  /** Wait this many seconds after the gate buy before an entry is allowed. */
  min_entry_s: number;
  /** Abort this cycle once the gate buy is older than this. */
  max_entry_s: number;
  /** Skip the entry while price is this far above the gate buy. 0 disables it. */
  max_chase: number;
  /** Take profit versus the fill. 0 disables it. */
  take_profit: number;
  /** Prints at or below this size are ignored by the buy streak. */
  dust_sol: number;
  /** Consecutive buys, above dust, that can trigger an exit. */
  sell_hit_count: number;
  /** Those buys must sum to at least this many SOL. */
  sell_hit_min: number;
  /** Exit when the target sells at least 80% of the tokens still held. */
  target_sell_exit: boolean;
  /** Live position size in SOL. */
  size_sol: number;
}

/**
 * Edit strategy knobs here (replaces scalpingbot config/strategy_v_011.json).
 * Entry is a wall-clock window after the target's buy, unless price has chased
 * too far. Exit is a take-profit versus the fill, the last `sell_hit_count`
 * buys in a row summing to `sell_hit_min`, or the target's max sell.
 * After that max sell, the target's next buy starts a new cycle.
 */
export const STRATEGY_V_011_CONFIG: StrategyV011Config = {
  gate_wallet: "4DdrfiDHpmx55i4SPssxVzS9ZaKLb8qr45NKY9Er9nNh",
  timer_ms: 200,
  min_entry_s: 4.5,
  max_entry_s: 12,
  max_chase: 0.20,
  take_profit: 0.10,
  dust_sol: 0.1,
  sell_hit_count: 4,
  sell_hit_min: 2.5,
  target_sell_exit: true,
  size_sol: 0.4
};

export function loadStrategyV011Config(): StrategyV011Config {
  const cfg = { ...STRATEGY_V_011_CONFIG };
  if (cfg.sell_hit_count < 1) cfg.sell_hit_count = 1;
  return cfg;
}
