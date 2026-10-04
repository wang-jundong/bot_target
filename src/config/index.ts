import "dotenv/config";
import bs58 from "bs58";
import { Keypair } from "@solana/web3.js";
import { envSchema, strategyWalletFields } from "./schema.js";
import { ACTIVE_STRATEGY, selectedStrategyNames, type StrategyName, TRADE } from "./trade.js";
import { loadStrategyV011Config } from "./strategyV011.js";
import { loadStrategyV022Config } from "./strategyV022.js";
import { solToLamports } from "../utils/bigint.js";
import { decryptTradingPrivateKey } from "../security/walletCrypto.js";

export type AppConfig = ReturnType<typeof loadConfig>;
export function authenticatedHeliusRpcUrl(rpcUrl: string, apiKey: string): string {
  const url = new URL(rpcUrl);
  if (url.hostname.endsWith("helius-rpc.com") && !url.searchParams.has("api-key")) url.searchParams.set("api-key", apiKey);
  return url.toString();
}

type WalletEnv = {
  WALLET_KEY_FILE: string;
  STRATEGY_V_011_PRIVATE_KEY_ENCRYPTED: string;
  STRATEGY_V_011_PRIVATE_KEY_BASE58: string;
  STRATEGY_V_022_PRIVATE_KEY_ENCRYPTED: string;
  STRATEGY_V_022_PRIVATE_KEY_BASE58: string;
};

function strategyPlan(
  name: StrategyName,
  strategyV011: ReturnType<typeof loadStrategyV011Config>,
  strategyV022: ReturnType<typeof loadStrategyV022Config>,
  env: WalletEnv
) {
  const sizeSol = name === "strategy_v_022" ? strategyV022.size_sol : strategyV011.size_sol;
  // strategy_v_011 has no market-cap gate. 0 leaves the buy-retry cap off.
  const maxEntryMarketCapSol = name === "strategy_v_022" ? strategyV022.max_mc_sol : 0;
  const buyAmountLamports = solToLamports(sizeSol.toFixed(9));
  const targetWallet = name === "strategy_v_022" ? strategyV022.gate_wallet : strategyV011.gate_wallet;
  if (buyAmountLamports <= 0n) throw new Error(`${name} size must be positive`);
  if (name === "strategy_v_022" && maxEntryMarketCapSol <= 0) throw new Error(`${name} max market cap must be positive`);
  if (targetWallet.trim().length < 32) throw new Error(`${name} gate_wallet is missing`);
  return { name, buyAmountLamports, maxEntryMarketCapSol, targetWallet: targetWallet.trim(), keypair: resolveTradingKeypair(name, env) };
}

function readPrivateKey(encrypted: string, plain: string, keyFile: string): string | undefined {
  const ciphertext = encrypted.trim();
  if (ciphertext) return decryptTradingPrivateKey(ciphertext, keyFile).trim();
  const base58 = plain.trim();
  return base58 || undefined;
}

export function resolveTradingKeypair(name: StrategyName, env: WalletEnv): Keypair {
  const fields = strategyWalletFields(name);
  const base58 = readPrivateKey(env[fields.encrypted], env[fields.plain], env.WALLET_KEY_FILE);
  if (!base58) throw new Error(`${name} has no trading wallet; set ${fields.encrypted}`);
  const secret = bs58.decode(base58);
  if (secret.length !== 64) throw new Error(`${name} trading private key must decode to 64 bytes`);
  return Keypair.fromSecretKey(secret);
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env) {
  const value = envSchema.parse(env);
  const strategyV011 = loadStrategyV011Config();
  const strategyV022 = loadStrategyV022Config();
  const strategy = ACTIVE_STRATEGY;
  const strategyPlans = selectedStrategyNames().map(name => strategyPlan(name, strategyV011, strategyV022, value));
  return Object.freeze({
    ...value,
    strategy,
    strategyPlans,
    strategyV011,
    strategyV022,
    buySlippageBps: TRADE.buySlippageBps,
    sellSlippageBps: TRADE.sellSlippageBps,
    HELIUS_RPC_URL: authenticatedHeliusRpcUrl(value.HELIUS_RPC_URL, value.HELIUS_API_KEY)
  });
}
