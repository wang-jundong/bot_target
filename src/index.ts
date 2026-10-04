import pino from "pino";
import { Connection } from "@solana/web3.js";
import { loadConfig } from "./config/index.js";
import { BlockhashManager } from "./execution/blockhashManager.js";
import { ConfirmationTracker } from "./execution/confirmationTracker.js";
import { LiveStrategyExecution } from "./execution/liveExecution.js";
import { MintTradeLock } from "./execution/mintTradeLock.js";
import { YellowstoneVibeClient } from "./grpc/vibeClient.js";
import { HeliusSender } from "./helius/sender.js";
import { RecoveryJournal } from "./recovery/journal.js";
import { PnlJournal } from "./pnl/pnlJournal.js";
import { TradingRuntime } from "./runtime.js";
import { StrategyV011Live } from "./strategy/strategyV011Live.js";
import { StrategyV022Live } from "./strategy/strategyV022Live.js";
import type { Strategy } from "./strategy/types.js";
import { PumpBondingCurveAdapter, PumpSwapAdapter } from "./venues/pumpAdapters.js";
import { PumpTradeDecoder } from "./venues/tradeDecoder.js";

const config = loadConfig();
const logger = pino({
  level: config.LOG_LEVEL,
  base: undefined,
  transport: { target: "pino-pretty", options: { colorize: false, translateTime: "SYS:standard", singleLine: true, ignore: "pid,hostname" } }
});
const connection = new Connection(config.HELIUS_RPC_URL, { commitment: "processed", confirmTransactionInitialTimeout: 15_000 });
const blockhashes = new BlockhashManager(connection, config.BLOCKHASH_REFRESH_MS);
const sender = new HeliusSender(config.HELIUS_SENDER_URL, config.HELIUS_SENDER_SWQOS_ONLY, true);
const confirmations = new ConfirmationTracker(connection);
const journal = new RecoveryJournal(config.RECOVERY_PATH);
const pnlJournal = new PnlJournal(config.PNL_PATH);
const vibe = new YellowstoneVibeClient({ endpoint: config.VIBE_GRPC_ENDPOINT, token: config.VIBE_GRPC_TOKEN }, error => {
  logger.error({ err: error instanceof Error ? error.message : String(error) }, "[02 STREAM] Vibe connection error");
});
const adapters = [
  new PumpBondingCurveAdapter(connection, config.COMPUTE_UNIT_LIMIT, config.PRIORITY_FEE_LAMPORTS, config.HELIUS_TIP_LAMPORTS),
  new PumpSwapAdapter(connection, config.COMPUTE_UNIT_LIMIT, config.PRIORITY_FEE_LAMPORTS, config.HELIUS_TIP_LAMPORTS)
];
let runtime!: TradingRuntime;
const canOpen = (state: Parameters<TradingRuntime["canOpenPosition"]>[0]) => runtime.canOpenPosition(state);
const mintTrades = new MintTradeLock();
const strategies = config.strategyPlans.map(plan => {
  const execution = new LiveStrategyExecution(
    connection,
    plan.keypair,
    plan.buyAmountLamports,
    config.buySlippageBps,
    config.sellSlippageBps,
    plan.maxEntryMarketCapSol,
    blockhashes,
    sender,
    confirmations,
    journal,
    pnlJournal,
    logger,
    plan.name,
    mintTrades
  );
  const strategy: Strategy = plan.name === "strategy_v_022"
    ? new StrategyV022Live(config.strategyV022, execution, plan.keypair, plan.targetWallet, config.buySlippageBps, config.sellSlippageBps, canOpen, logger)
    : new StrategyV011Live(config.strategyV011, execution, plan.keypair, plan.targetWallet, config.buySlippageBps, config.sellSlippageBps, canOpen, logger);
  execution.bindStrategy(strategy);
  return { name: plan.name, strategy, buyAmountLamports: plan.buyAmountLamports, targetWallet: plan.targetWallet, owner: plan.keypair.publicKey };
});
runtime = new TradingRuntime(config, connection, vibe, journal, new PumpTradeDecoder(connection), adapters, logger, strategies);

const strategyKnobs = Object.fromEntries(config.strategyPlans.map(plan => [plan.name, plan.name === "strategy_v_022"
  ? {
      targetWallet: config.strategyV022.gate_wallet,
      sizeSol: config.strategyV022.size_sol,
      dustSol: config.strategyV022.dust_sol,
      buyHitCount: config.strategyV022.buy_hit_count,
      buyHitMin: config.strategyV022.buy_hit_min,
      buyHitRound: config.strategyV022.buy_hit_round,
      sellHitCount: config.strategyV022.sell_hit_count,
      sellHitMin: config.strategyV022.sell_hit_min,
      maxMcSol: config.strategyV022.max_mc_sol,
      takeProfit: config.strategyV022.take_profit,
      stopLoss: config.strategyV022.stop_loss,
      targetSellExit: config.strategyV022.target_sell_exit
    }
  : {
      targetWallet: config.strategyV011.gate_wallet,
      sizeSol: config.strategyV011.size_sol,
      minEntryS: config.strategyV011.min_entry_s,
      maxEntryS: config.strategyV011.max_entry_s,
      maxChase: config.strategyV011.max_chase,
      takeProfit: config.strategyV011.take_profit,
      dustSol: config.strategyV011.dust_sol,
      sellHitCount: config.strategyV011.sell_hit_count,
      sellHitMin: config.strategyV011.sell_hit_min,
      targetSellExit: config.strategyV011.target_sell_exit,
      timerMs: config.strategyV011.timer_ms
    }
]));

logger.info({
  strategy: config.strategy,
  executionMode: config.EXECUTION_MODE,
  ...strategyKnobs,
  pnlPath: config.PNL_PATH,
  logLevel: config.LOG_LEVEL
}, "[01 STARTUP] Configuration loaded");

blockhashes.start(error => logger.error({ err: error instanceof Error ? error.message : String(error) }, "[01 STARTUP] Blockhash refresh failed"));
await runtime.start();
logger.info({
  wallets: Object.fromEntries(config.strategyPlans.map(plan => [plan.name, plan.keypair.publicKey.toBase58()])),
  strategy: config.strategy,
  swqosOnly: config.HELIUS_SENDER_SWQOS_ONLY
}, `[01 STARTUP] Bot ready; ${config.strategy} active`);

let closing = false;
async function shutdown(): Promise<void> {
  if (closing) return;
  closing = true;
  blockhashes.stop();
  await runtime.close();
  await sender.close();
}
process.once("SIGINT", () => void shutdown().finally(() => process.exit(0)));
process.once("SIGTERM", () => void shutdown().finally(() => process.exit(0)));
