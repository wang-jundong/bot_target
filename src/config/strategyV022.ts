export interface StrategyV022Config {
  gate_wallet: string;
  dust_sol: number;
  buy_hit_count: number;
  buy_hit_min: number;
  buy_hit_round: number;
  sell_hit_count: number;
  sell_hit_min: number;
  max_mc_sol: number;
  stop_loss: number;
  take_profit: number;
  target_sell_exit: boolean;
  size_sol: number;
}

/**
 * Edit strategy knobs here (replaces scalpingbot config/strategy_v_022.json).
 * Train #1 book. Buy is qualifying sell round `buy_hit_round` (2 = the second round)
 * of `buy_hit_count` sells summing to at least `buy_hit_min`, at a market cap of
 * `max_mc_sol` or less. Earlier qualifying rounds are skipped. A short round is
 * dropped and the next sells start a new one. A partial sell does not clear the bag.
 */
export const STRATEGY_V_022_CONFIG: StrategyV022Config = {
  gate_wallet: "FqamE7xrahg7FEWoByrx1o8SeyHt44rpmE6ZQfT7zrve",
  dust_sol: 0.1,
  buy_hit_count: 4,
  buy_hit_min: 1.0,
  buy_hit_round: 2,
  sell_hit_count: 3,
  sell_hit_min: 6.0,
  max_mc_sol: 90.0,
  stop_loss: 0.0,
  take_profit: 0.15,
  target_sell_exit: true,
  size_sol: 0.6
};

export function loadStrategyV022Config(): StrategyV022Config {
  const cfg = { ...STRATEGY_V_022_CONFIG };
  if (cfg.buy_hit_count < 1) cfg.buy_hit_count = 1;
  if (cfg.buy_hit_round < 1) cfg.buy_hit_round = 1;
  if (cfg.sell_hit_count < 1) cfg.sell_hit_count = 1;
  return cfg;
}
