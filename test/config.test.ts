import { describe, expect, it } from "vitest";
import bs58 from "bs58";
import { Keypair } from "@solana/web3.js";
import { authenticatedHeliusRpcUrl, resolveTradingKeypair } from "../src/config/index.js";

const emptyWalletEnv = {
  WALLET_KEY_FILE: "./secrets/solana.key",
  STRATEGY_V_011_PRIVATE_KEY_ENCRYPTED: "",
  STRATEGY_V_011_PRIVATE_KEY_BASE58: "",
  STRATEGY_V_022_PRIVATE_KEY_ENCRYPTED: "",
  STRATEGY_V_022_PRIVATE_KEY_BASE58: "",
  STRATEGY_V_031_PRIVATE_KEY_ENCRYPTED: "",
  STRATEGY_V_031_PRIVATE_KEY_BASE58: ""
};

describe("Helius RPC authentication", () => {
  it("adds the configured API key to a Helius base URL", () => {
    expect(authenticatedHeliusRpcUrl("https://mainnet.helius-rpc.com/", "secret"))
      .toBe("https://mainnet.helius-rpc.com/?api-key=secret");
  });
  it("preserves an explicitly configured API key", () => {
    expect(authenticatedHeliusRpcUrl("https://mainnet.helius-rpc.com/?api-key=explicit", "secret"))
      .toBe("https://mainnet.helius-rpc.com/?api-key=explicit");
  });
  it("does not leak the key to a non-Helius endpoint", () => {
    expect(authenticatedHeliusRpcUrl("https://rpc.example.com/", "secret"))
      .toBe("https://rpc.example.com/");
  });
});

describe("per-strategy trading wallets", () => {
  it("loads each strategy from its own key", () => {
    const first = Keypair.generate();
    const second = Keypair.generate();
    const env = {
      ...emptyWalletEnv,
      STRATEGY_V_011_PRIVATE_KEY_BASE58: bs58.encode(first.secretKey),
      STRATEGY_V_022_PRIVATE_KEY_BASE58: bs58.encode(second.secretKey)
    };
    expect(resolveTradingKeypair("strategy_v_011", env).publicKey.toBase58()).toBe(first.publicKey.toBase58());
    expect(resolveTradingKeypair("strategy_v_022", env).publicKey.toBase58()).toBe(second.publicKey.toBase58());
  });

  it("rejects a strategy that has no key", () => {
    expect(() => resolveTradingKeypair("strategy_v_022", emptyWalletEnv)).toThrow(/strategy_v_022/);
  });
});
