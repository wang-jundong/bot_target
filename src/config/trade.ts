/**
 * Which strategy this process runs, plus the shared order settings.
 * Edit these here. They are not environment variables.
 * "both" runs strategy_v_011, strategy_v_022, and strategy_v_031 together.
 * Each keeps its own position and signs with its own wallet.
 * Position size and market-cap cap come from the selected strategy file.
 */
export const STRATEGY_NAMES = ["strategy_v_011", "strategy_v_022", "strategy_v_031"] as const;
export type StrategyName = (typeof STRATEGY_NAMES)[number];
export const ACTIVE_STRATEGY: StrategyName | "both" = "both";

export function selectedStrategyNames(): readonly StrategyName[] {
  return ACTIVE_STRATEGY === "both" ? STRATEGY_NAMES : [ACTIVE_STRATEGY];
}

export const TRADE = {
  buySlippageBps: 1200,
  sellSlippageBps: 5000
} as const;
