import { describe, expect, it } from "vitest";
import { catchUpReplayGap, parsedTargetFromRpc, type ReplayCatchUpSource } from "../src/grpc/replayCatchUp.js";
import type { ParsedTargetTransaction } from "../src/venues/types.js";

describe("replay catch-up", () => {
  it("returns trades at or after the missed slot, oldest first, and skips failures", async () => {
    const txs = new Map<string, ParsedTargetTransaction>([
      ["sell", { signature: "sell", slot: 105, timestampMs: 2, accountKeys: ["pool"], programIds: ["pump"], raw: {} }],
      ["late", { signature: "late", slot: 110, timestampMs: 3, accountKeys: ["wallet"], programIds: ["pump"], raw: {} }]
    ]);
    const source: ReplayCatchUpSource = {
      signatures: async account => account === "wallet"
        ? [
          { signature: "late", slot: 110, err: null },
          { signature: "failed", slot: 108, err: { InstructionError: [0, { Custom: 1 }] } },
          { signature: "sell", slot: 105, err: null },
          { signature: "before", slot: 99, err: null }
        ]
        : [{ signature: "sell", slot: 105, err: null }],
      transaction: async signature => txs.get(signature)
    };
    const delivered: string[] = [];
    const count = await catchUpReplayGap(
      { fromSlot: 100, accounts: ["wallet", "pool"] },
      source,
      tx => delivered.push(tx.signature)
    );
    expect(count).toBe(2);
    expect(delivered).toEqual(["sell", "late"]);
  });

  it("parses an RPC transaction into the shape the trade decoder reads", () => {
    const parsed = parsedTargetFromRpc({
      slot: 454343671,
      blockTime: 1_791_408_629,
      transaction: {
        signatures: ["sig"],
        message: {
          accountKeys: ["wallet", "pool"],
          instructions: [{ programIdIndex: 1 }]
        }
      },
      meta: {
        err: null,
        logMessages: ["Program data: abc"],
        loadedAddresses: { writable: ["loaded"], readonly: [] }
      }
    });
    expect(parsed).toMatchObject({
      signature: "sig",
      slot: 454343671,
      timestampMs: 1_791_408_629_000,
      accountKeys: ["wallet", "pool", "loaded"],
      programIds: ["pool"]
    });
    expect(parsed?.raw).toMatchObject({ meta: { logMessages: ["Program data: abc"] } });
  });
});
