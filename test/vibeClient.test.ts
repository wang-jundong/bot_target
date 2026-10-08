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
          transaction: { message: { accountKeys: [account], instructions: [] } },
          meta: { loadedWritableAddresses: [], loadedReadonlyAddresses: [] }
        }
      }
    }
  } as unknown as ReconnectEvent;
}

class FakeStream extends EventEmitter {
  readonly writes: SubscribeRequest[] = [];

  write(request: SubscribeRequest, callback: (error?: Error | null) => void): boolean {
    this.writes.push(request);
    callback();
    return true;
  }

  destroy(): this {
    this.emit("close");
    return this;
  }
}

function openedWith(subscribe: { mock: { calls: unknown[][] } }): SubscribeRequest[] {
  return subscribe.mock.calls.map(call => call[0] as SubscribeRequest);
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

    try {
      await client.subscribeWallet(wallet, () => undefined);
      original.emit("data", update("452088739", bs58.decode(wallet)));
      original.emit("error", Object.assign(new Error("14 UNAVAILABLE: Connection dropped"), { code: 14 }));
      await vi.advanceTimersByTimeAsync(0);
      const requests = openedWith(transport.subscribeWithReconnect);
      expect(requests).toHaveLength(2);
      expect(requests.every(request => request.fromSlot === undefined)).toBe(true);
      expect(requests.every(request => request.transactions?.tracked?.accountInclude?.length === 1)).toBe(true);
      expect(transport.subscribeWithReconnect).toHaveBeenCalledTimes(2);
      expect(errors).toHaveLength(1);
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

    try {
      await client.subscribeWallet(wallet, tx => order.push(`tx:${tx.slot}`));
      original.emit("error", Object.assign(new Error("14 UNAVAILABLE: Connection dropped"), { code: 14 }));
      await vi.advanceTimersByTimeAsync(0);
      live.emit("data", update("452088740", bs58.decode(wallet), new Uint8Array(64).fill(2)));
      expect(order).toEqual(["tx:452088740"]);
      expect(openedWith(transport.subscribeWithReconnect).every(request => request.fromSlot === undefined)).toBe(true);
    } finally {
      await client.close();
    }
  });

  it("opens a pool on its own stream without fromSlot", async () => {
    const streams = [new FakeStream(), new FakeStream()];
    const transport = {
      connect: async () => undefined,
      subscribeWithReconnect: vi.fn(async () => streams.shift()!)
    };
    const client = new YellowstoneVibeClient({ endpoint: "test", token: "" }, () => undefined, transport);
    const wallet = "11111111111111111111111111111111";

    try {
      await client.subscribeWallet(wallet, () => undefined);
      await client.subscribePool(
        { mint: "mint", pool: "pool", programId: "program", venue: "pump", relevantAccounts: [] },
        () => undefined
      );
      const requests = openedWith(transport.subscribeWithReconnect);
      expect(requests).toHaveLength(2);
      expect(requests[0]?.transactions?.tracked?.accountInclude).toEqual([wallet]);
      expect(requests[1]?.transactions?.tracked?.accountInclude).toEqual(["pool"]);
      expect(requests.every(request => request.fromSlot === undefined)).toBe(true);
    } finally {
      await client.close();
    }
  });

  it("stays on the same stream when version 9 discards a bank", async () => {
    const stream = new FakeStream();
    const transport = {
      connect: async () => undefined,
      subscribeWithReconnect: vi.fn(async () => stream)
    };
    const errors: unknown[] = [];
    const client = new YellowstoneVibeClient({ endpoint: "test", token: "" }, error => errors.push(error), transport);
    const wallet = "11111111111111111111111111111111";

    try {
      await client.subscribeWallet(wallet, () => undefined);
      stream.emit("data", {
        type: "DiscardBanks",
        banks: [{ generation: "1", slot: "452088739", bankId: "1" }],
        reason: "IncompleteDelivery",
        replacement: { fromSlot: "452088739", generation: "2" },
        winners: [{ type: "Unknown", slot: "452088739" }]
      } as unknown as ReconnectEvent);
      expect(transport.subscribeWithReconnect).toHaveBeenCalledTimes(1);
      expect(errors).toHaveLength(0);
    } finally {
      await client.close();
    }
  });

  it("answers a server ping without changing the filter", async () => {
    const stream = new FakeStream();
    const transport = {
      connect: async () => undefined,
      subscribeWithReconnect: vi.fn(async () => stream)
    };
    const client = new YellowstoneVibeClient({ endpoint: "test", token: "" }, () => undefined, transport);
    const wallet = "11111111111111111111111111111111";

    try {
      await client.subscribeWallet(wallet, () => undefined);
      stream.emit("data", { type: "Update", generation: "1", update: { ping: { id: 1 } } } as unknown as ReconnectEvent);
      await vi.waitFor(() => expect(stream.writes).toHaveLength(1));
      expect(stream.writes[0]?.ping).toEqual({ id: 1 });
      expect(stream.writes[0]?.fromSlot).toBeUndefined();
      expect(stream.writes[0]?.transactions).toEqual({});
      expect(transport.subscribeWithReconnect).toHaveBeenCalledTimes(1);
    } finally {
      await client.close();
    }
  });
});
