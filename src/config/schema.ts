import { z } from "zod";
import { selectedStrategyNames, type StrategyName } from "./trade.js";

const numeric = (min = -Number.MAX_VALUE) => z.coerce.number().finite().min(min);
const integer = (min = 0) => z.coerce.number().int().min(min);
const bool = z.enum(["true", "false"]).transform(v => v === "true");

export const envSchema = z.object({
  EXECUTION_MODE: z.literal("live").default("live"),
  BLOCKED_MINTS: z.string().default(""),
  VIBE_GRPC_ENDPOINT: z.string().min(1),
  VIBE_GRPC_TOKEN: z.string().min(1),
  HELIUS_API_KEY: z.string().min(1),
  HELIUS_RPC_URL: z.string().url(),
  HELIUS_SENDER_URL: z.string().url(),
  HELIUS_SENDER_SWQOS_ONLY: bool.default("true"),
  /** Fernet key file path (same role as cryptotrading_prod SIG_KEYS[*].key_file). */
  WALLET_KEY_FILE: z.string().min(1).default("./secrets/solana.key"),
  /** Fernet ciphertext of XOR(private_key) for strategy_v_011. */
  STRATEGY_V_011_PRIVATE_KEY_ENCRYPTED: z.string().optional().default(""),
  /** Legacy plaintext base58 — only used if that strategy's ENCRYPTED value is empty. */
  STRATEGY_V_011_PRIVATE_KEY_BASE58: z.string().optional().default(""),
  /** Fernet ciphertext of XOR(private_key) for strategy_v_022. */
  STRATEGY_V_022_PRIVATE_KEY_ENCRYPTED: z.string().optional().default(""),
  STRATEGY_V_022_PRIVATE_KEY_BASE58: z.string().optional().default(""),
  /** Fernet ciphertext of XOR(private_key) for strategy_v_031. */
  STRATEGY_V_031_PRIVATE_KEY_ENCRYPTED: z.string().optional().default(""),
  STRATEGY_V_031_PRIVATE_KEY_BASE58: z.string().optional().default(""),
  PRIORITY_FEE_LAMPORTS: integer(1),
  HELIUS_TIP_LAMPORTS: integer(5000).default(5000),
  COMPUTE_UNIT_LIMIT: integer(1),
  EVENT_RETENTION_SEC: numeric(1).default(180),
  BLOCKHASH_REFRESH_MS: integer(1000).default(10000),
  LOG_LEVEL: z.string().default("info"),
  RECOVERY_PATH: z.string().default("./recovery/lifecycle.jsonl"),
  PNL_PATH: z.string().min(1).default("./logs/pnl.jsonl")
}).superRefine((v, ctx) => {
  if (!v.HELIUS_SENDER_SWQOS_ONLY && v.HELIUS_TIP_LAMPORTS < 1_000_000) {
    ctx.addIssue({ code: "custom", path: ["HELIUS_TIP_LAMPORTS"], message: "Sender Max requires at least 1000000 tip lamports" });
  }
  for (const name of selectedStrategyNames()) {
    const fields = strategyWalletFields(name);
    if (!v[fields.encrypted].trim() && !v[fields.plain].trim()) {
      ctx.addIssue({
        code: "custom",
        path: [fields.encrypted],
        message: `set ${fields.encrypted} (+ WALLET_KEY_FILE) or ${fields.plain}`
      });
    }
  }
});

const STRATEGY_WALLET_FIELDS = {
  strategy_v_011: { encrypted: "STRATEGY_V_011_PRIVATE_KEY_ENCRYPTED", plain: "STRATEGY_V_011_PRIVATE_KEY_BASE58" },
  strategy_v_022: { encrypted: "STRATEGY_V_022_PRIVATE_KEY_ENCRYPTED", plain: "STRATEGY_V_022_PRIVATE_KEY_BASE58" },
  strategy_v_031: { encrypted: "STRATEGY_V_031_PRIVATE_KEY_ENCRYPTED", plain: "STRATEGY_V_031_PRIVATE_KEY_BASE58" }
} as const;

export function strategyWalletFields(name: StrategyName): (typeof STRATEGY_WALLET_FIELDS)[StrategyName] {
  return STRATEGY_WALLET_FIELDS[name];
}
