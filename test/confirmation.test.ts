import { describe, expect, it, vi } from "vitest";
import { PublicKey, type Connection } from "@solana/web3.js";
import { ConfirmationTimeoutError, ConfirmationTracker } from "../src/execution/confirmationTracker.js";
import type { VenueAdapter } from "../src/venues/types.js";

const owner = new PublicKey("11111111111111111111111111111111");
const adapter = { parseFill: vi.fn(() => ({ tokenAmount: 2n, solAmount: 1n, price: 0.5, success: true })) } as unknown as VenueAdapter;

describe("confirmation polling", () => {
  it("waits until a confirmed transaction is retrievable", async () => {
    const getSignatureStatuses = vi.fn()
      .mockResolvedValueOnce({ value: [{ err: null, confirmationStatus: "processed" }] })
      .mockResolvedValueOnce({ value: [{ err: null, confirmationStatus: "confirmed" }] });
    const getTransaction = vi.fn().mockResolvedValue({ meta: { err: null } });
    const tracker = new ConfirmationTracker({ getSignatureStatuses, getTransaction } as unknown as Connection, 2_000);
    await expect(tracker.waitProcessed("sig", adapter, owner, "mint")).resolves.toMatchObject({ success: true, tokenAmount: 2n });
    expect(getTransaction).toHaveBeenCalledOnce();
  });

  it("distinguishes an observation timeout from an on-chain failure", async () => {
    const timeout = new ConfirmationTracker({ getSignatureStatuses: vi.fn() } as unknown as Connection, 0);
    await expect(timeout.waitProcessed("sig", adapter, owner, "mint")).rejects.toBeInstanceOf(ConfirmationTimeoutError);
    const failed = new ConfirmationTracker({ getSignatureStatuses: vi.fn().mockResolvedValue({ value: [{ err: { InstructionError: [0, "Custom"] }, confirmationStatus: "processed" }] }) } as unknown as Connection);
    await expect(failed.waitProcessed("sig", adapter, owner, "mint")).rejects.toThrow("transaction failed");
  });

  it("stops when a fallback signature is never seen", async () => {
    const getSignatureStatuses = vi.fn().mockResolvedValue({ value: [null] });
    const tracker = new ConfirmationTracker({ getSignatureStatuses } as unknown as Connection, 30_000);
    await expect(tracker.waitProcessed("sig", adapter, owner, "mint", 0)).rejects.toMatchObject({ seen: false });
    expect(getSignatureStatuses).toHaveBeenCalledOnce();
  });

  it("keeps polling a signature the cluster has already seen", async () => {
    const getSignatureStatuses = vi.fn().mockResolvedValue({ value: [{ err: null, confirmationStatus: "processed" }] });
    const tracker = new ConfirmationTracker({ getSignatureStatuses } as unknown as Connection, 600);
    await expect(tracker.waitProcessed("sig", adapter, owner, "mint", 0)).rejects.toMatchObject({ seen: true });
    expect(getSignatureStatuses.mock.calls.length).toBeGreaterThan(1);
  });
});
