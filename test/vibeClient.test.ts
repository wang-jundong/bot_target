import { EventEmitter } from "node:events";
import bs58 from "bs58";
import type { SubscribeRequest, SubscribeUpdate } from "@triton-one/yellowstone-grpc";
import { afterEach, describe, expect, it, vi } from "vitest";
import { YellowstoneVibeClient } from "../src/grpc/vibeClient.js";
import type { PoolDescriptor } from "../src/venues/types.js";

class FakeStream extends EventEmitter {
  readonly writes: SubscribeRequest[] = [];

  write(request: SubscribeRequest, callback: (error?: Error | null) => void): boolean {
    this.writes.push(request);
    callback();
    return true;
  }

  cancel(): void { this.emit("close"); }
}

afterEach(() => vi.useRealTimers());

describe("Vibe stream reconnect", () => {
  it("drops an unavailable replay slot and reconnects to live transactions", async () => {
    vi.useFakeTimers();
    const streams = [new FakeStream(), new FakeStream(), new FakeStream()];
    const errors: unknown[] = [];
    const transport = {
      subscribe: vi.fn(async () => streams.shift()!),
      ping: vi.fn(async () => 1)
    };
    const client = new YellowstoneVibeClient({ endpoint: "test", token: "" }, error => errors.push(error), transport);
    const gaps: { fromSlot: number; accounts: readonly string[] }[] = [];
    client.setReplayGapHandler(gap => gaps.push(gap));
    const wallet = "11111111111111111111111111111111";
    const original = streams[0]!;
    const replay = streams[1]!;
    const live = streams[2]!;

    try {
      await client.subscribeWallet(wallet, () => undefined);
      original.emit("data", {
        transaction: {
          slot: "452088739",
          transaction: {
            signature: new Uint8Array(64),
            transaction: { message: { accountKeys: [bs58.decode(wallet)], instructions: [] } },
            meta: { loadedWritableAddresses: [], loadedReadonlyAddresses: [] }
          }
        }
      } as unknown as SubscribeUpdate);

      original.emit("error", Object.assign(new Error("14 UNAVAILABLE: Connection dropped"), { code: 14 }));
      await vi.advanceTimersByTimeAsync(2_000);
      expect(replay.writes[0]?.fromSlot).toBe("452088739");

      replay.emit("error", Object.assign(new Error("11 OUT_OF_RANGE: broadcast from 452088739 is not available"), { code: 11 }));
      expect(gaps).toEqual([]);
      await vi.advanceTimersByTimeAsync(2_000);
      expect(live.writes[0]?.fromSlot).toBeUndefined();
      expect(gaps).toEqual([{ fromSlot: 452088739, accounts: [wallet] }]);
      expect(transport.subscribe).toHaveBeenCalledTimes(3);
      expect(errors).toHaveLength(2);
    } finally {
      await client.close();
    }
  });

  it("keeps delivering wallet transactions when a pool replay is rejected", async () => {
    vi.useFakeTimers();
    const streams = [new FakeStream(), new FakeStream()];
    const transport = {
      subscribe: vi.fn(async () => streams.shift()!),
      ping: vi.fn(async () => 1)
    };
    const client = new YellowstoneVibeClient({ endpoint: "test", token: "" }, () => undefined, transport);
    const gaps: { fromSlot: number; accounts: readonly string[] }[] = [];
    client.setReplayGapHandler(gap => gaps.push(gap));
    const wallet = "11111111111111111111111111111111";
    const pool = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    const live = streams[0]!;
    const replay = streams[1]!;
    const seen: string[] = [];
    const descriptor = { mint: pool, pool, programId: pool, venue: "pump", relevantAccounts: [pool] } as PoolDescriptor;

    try {
      await client.subscribeWallet(wallet, tx => seen.push(tx.signature));
      await client.subscribePool(descriptor, () => undefined, 100);
      expect(live.writes.length).toBeGreaterThan(0);
      expect(live.writes.every(write => write.fromSlot === undefined)).toBe(true);
      expect(replay.writes[0]?.fromSlot).toBe("100");
      expect(replay.writes[0]?.transactions.tracked?.accountInclude).toEqual(expect.arrayContaining([wallet, pool]));

      replay.emit("error", Object.assign(new Error("13 INTERNAL: failed to get replay response"), { code: 13 }));
      expect(gaps).toEqual([{ fromSlot: 100, accounts: [wallet, pool] }]);
      await vi.advanceTimersByTimeAsync(2_000);
      expect(transport.subscribe).toHaveBeenCalledTimes(2);

      live.emit("data", {
        transaction: {
          slot: "452088800",
          transaction: {
            signature: new Uint8Array(64),
            transaction: { message: { accountKeys: [bs58.decode(wallet)], instructions: [] } },
            meta: { loadedWritableAddresses: [], loadedReadonlyAddresses: [] }
          }
        }
      } as unknown as SubscribeUpdate);
      expect(seen).toHaveLength(1);
    } finally {
      await client.close();
    }
  });

  it("drops a replay slot when Vibe returns status 13 and reconnects live", async () => {
    vi.useFakeTimers();
    const streams = [new FakeStream(), new FakeStream(), new FakeStream()];
    const errors: unknown[] = [];
    const transport = {
      subscribe: vi.fn(async () => streams.shift()!),
      ping: vi.fn(async () => 1)
    };
    const client = new YellowstoneVibeClient({ endpoint: "test", token: "" }, error => errors.push(error), transport);
    const gaps: { fromSlot: number; accounts: readonly string[] }[] = [];
    client.setReplayGapHandler(gap => gaps.push(gap));
    const wallet = "11111111111111111111111111111111";
    const original = streams[0]!;
    const replay = streams[1]!;
    const live = streams[2]!;

    try {
      await client.subscribeWallet(wallet, () => undefined);
      original.emit("data", {
        transaction: {
          slot: "452088739",
          transaction: {
            signature: new Uint8Array(64),
            transaction: { message: { accountKeys: [bs58.decode(wallet)], instructions: [] } },
            meta: { loadedWritableAddresses: [], loadedReadonlyAddresses: [] }
          }
        }
      } as unknown as SubscribeUpdate);

      original.emit("error", Object.assign(new Error("14 UNAVAILABLE: Connection dropped"), { code: 14 }));
      await vi.advanceTimersByTimeAsync(2_000);
      expect(replay.writes[0]?.fromSlot).toBe("452088739");

      replay.emit("error", Object.assign(new Error("13 INTERNAL: failed to get replay response"), { code: 13, details: "failed to get replay response" }));
      expect(gaps).toEqual([]);
      await vi.advanceTimersByTimeAsync(2_000);
      expect(live.writes[0]?.fromSlot).toBeUndefined();
      expect(gaps).toEqual([{ fromSlot: 452088739, accounts: [wallet] }]);
      expect(transport.subscribe).toHaveBeenCalledTimes(3);
      expect(errors).toHaveLength(2);
    } finally {
      await client.close();
    }
  });
});
