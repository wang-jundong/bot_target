import type { Connection, PublicKey } from "@solana/web3.js";
import type { FillResult, VenueAdapter } from "../venues/types.js";

const wait = (ms: number): Promise<void> => new Promise(resolve => setTimeout(resolve, ms));

export class ConfirmationTimeoutError extends Error {
  constructor(message: string, readonly seen: boolean) {
    super(message);
    this.name = "ConfirmationTimeoutError";
  }
}

export class ConfirmationTracker {
  constructor(private readonly connection: Connection, private readonly timeoutMs = 30_000) {}

  async waitProcessed(signature: string, adapter: VenueAdapter, owner: PublicKey, mint: string, unseenTimeoutMs = this.timeoutMs): Promise<FillResult> {
    const started = Date.now();
    const deadline = started + this.timeoutMs;
    const unseenDeadline = started + Math.min(this.timeoutMs, unseenTimeoutMs);
    let seen = false;
    while (Date.now() < deadline) {
      const status = (await this.connection.getSignatureStatuses([signature], { searchTransactionHistory: true })).value[0];
      if (status) seen = true;
      if (status?.err) throw new Error(`transaction failed: ${JSON.stringify(status.err)}`);
      if (status?.confirmationStatus === "confirmed" || status?.confirmationStatus === "finalized") {
        const tx = await this.connection.getTransaction(signature, { commitment: "confirmed", maxSupportedTransactionVersion: 0 });
        if (tx?.meta?.err) throw new Error(`transaction failed: ${JSON.stringify(tx.meta.err)}`);
        if (tx) return adapter.parseFill(tx, owner, mint);
      } else if (!seen && Date.now() >= unseenDeadline) break;
      await wait(500);
    }
    throw new ConfirmationTimeoutError(`confirmation timed out for ${signature}`, seen);
  }

  async checkConfirmed(signature: string): Promise<boolean> {
    const status = (await this.connection.getSignatureStatuses([signature], { searchTransactionHistory: true })).value[0];
    return !!status && status.err === null && (status.confirmationStatus === "confirmed" || status.confirmationStatus === "finalized");
  }
}
