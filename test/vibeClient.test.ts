import { EventEmitter } from "node:events";
import bs58 from "bs58";
import type { ReconnectEvent, SubscribeRequest } from "@triton-one/yellowstone-grpc";
import { afterEach, describe, expect, it, vi } from "vitest";
import { YellowstoneVibeClient } from "../src/grpc/vibeClient.js";

function update(slot: string, account: Uint8Array, signature = new Uint8Array(64)): ReconnectEvent {
  return {
    type: "Update",
    generation: "1",
    update: {
      filters: [],
      createdAt: undefined,
      transaction: {
        slot,
        bankId: "1",
        transaction: {
          signature,
          isVote: false,
          index: "0",
          transaction: { message: { accountKeys: [account], instructions: [] } },
          meta: { loadedWritableAddresses: [], loadedReadonlyAddresses: [] }
        }
      }
    }
  } as unknown as ReconnectEvent;
}

class FakeStream extends EventEmitter {
  readonly writes: SubscribeRequest[] = [];
  beforeWrite?: () => void;

  write(request: SubscribeRequest, callback: (error?: Error | null) => void): boolean {
    this.writes.push(request);
    this.beforeWrite?.();
    callback();
    return true;
  }

  cancel(): void { this.emit("close"); }
}

afterEach(() => vi.useRealTimers());

describe("Vibe stream reconnect", () => {
  it("reconnects live and never asks Vibe to replay", async () => {
    vi.useFakeTimers();
    const streams = [new FakeStream(), new FakeStream()];
    const errors: unknown[] = [];
    const transport = {
      connect: async () => undefined,
      subscribeWithReconnect: vi.fn(async () => streams.shift()!)
    };
    const client = new YellowstoneVibeClient({ endpoint: "test", token: "" }, error => errors.push(error), transport);
    const wallet = "11111111111111111111111111111111";
    const original = streams[0]!;
    const live = streams[1]!;

    try {
      await client.subscribeWallet(wallet, () => undefined);
      original.emit("data", update("452088739", bs58.decode(wallet)));

      original.emit("error", Object.assign(new Error("14 UNAVAILABLE: Connection dropped"), { code: 14 }));
      await vi.advanceTimersByTimeAsync(0);
      expect(live.writes[0]?.fromSlot).toBeUndefined();
      expect(live.writes[0]?.transactions?.tracked?.accountInclude).toEqual([wallet]);
      expect(transport.subscribeWithReconnect).toHaveBeenCalledTimes(2);
      expect(errors).toHaveLength(1);
      expect([...original.writes, ...live.writes].every(write => write.fromSlot === undefined)).toBe(true);
    } finally {
      await client.close();
    }
  });

  it("delivers a transaction that arrives on the reconnected stream", async () => {
    vi.useFakeTimers();
    const streams = [new FakeStream(), new FakeStream()];
    const transport = {
      connect: async () => undefined,
      subscribeWithReconnect: vi.fn(async () => streams.shift()!)
    };
    const order: string[] = [];
    const client = new YellowstoneVibeClient({ endpoint: "test", token: "" }, () => undefined, transport);
    const wallet = "11111111111111111111111111111111";
    const original = streams[0]!;
    const live = streams[1]!;
    live.beforeWrite = () => {
      live.emit("data", update("452088740", bs58.decode(wallet), new Uint8Array(64).fill(2)));
    };

    try {
      await client.subscribeWallet(wallet, tx => order.push(`tx:${tx.slot}`));
      original.emit("error", Object.assign(new Error("14 UNAVAILABLE: Connection dropped"), { code: 14 }));
      await vi.advanceTimersByTimeAsync(0);
      expect(order).toEqual(["tx:452088740"]);
      expect(live.writes.every(write => write.fromSlot === undefined)).toBe(true);
    } finally {
      await client.close();
    }
  });

  it("adds a pool to the live filter without fromSlot", async () => {
    const streams = [new FakeStream()];
    const transport = {
      connect: async () => undefined,
      subscribeWithReconnect: vi.fn(async () => streams.shift()!)
    };
    const client = new YellowstoneVibeClient({ endpoint: "test", token: "" }, () => undefined, transport);
    const wallet = "11111111111111111111111111111111";
    const live = streams[0]!;

    try {
      await client.subscribeWallet(wallet, () => undefined);
      await client.subscribePool(
        { mint: "mint", pool: "pool", programId: "program", venue: "pump", relevantAccounts: [] },
        () => undefined
      );
      expect(transport.subscribeWithReconnect).toHaveBeenCalledTimes(1);
      expect(live.writes.at(-1)?.transactions?.tracked?.accountInclude).toEqual([wallet, "pool"]);
      expect(live.writes.every(write => write.fromSlot === undefined)).toBe(true);
    } finally {
      await client.close();
    }
  });
});
