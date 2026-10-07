/**
 * Which strategies this process runs, plus the shared order settings.
 * Edit these here. They are not environment variables.
 * Each active strategy keeps its own position and signs with its own wallet.
 * Position size and market-cap cap come from the selected strategy file.
 */
export const STRATEGY_NAMES = ["strategy_v_011", "strategy_v_022", "strategy_v_031"] as const;
export type StrategyName = (typeof STRATEGY_NAMES)[number];

/** Names listed here are the ones that trade. */
export const ACTIVE_STRATEGIES = ["strategy_v_011", "strategy_v_022", "strategy_v_031"] as const satisfies readonly StrategyName[];

export function selectedStrategyNames(): readonly StrategyName[] {
  return ACTIVE_STRATEGIES;
}

export const TRADE = {
  buySlippageBps: 1200,
  sellSlippageBps: 5000
} as const;
