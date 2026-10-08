import type { Connection } from "@solana/web3.js";
import type { ReplayGap } from "./vibeClient.js";
import type { ParsedTargetTransaction } from "../venues/types.js";

const PAGE_SIZE = 100;
const MAX_PAGES = 5;

export interface SignaturePageEntry {
  signature: string;
  slot: number;
  err: unknown;
}

export interface ReplayCatchUpSource {
  signatures(account: string, before?: string): Promise<readonly SignaturePageEntry[]>;
  transaction(signature: string): Promise<ParsedTargetTransaction | undefined>;
}

interface RpcTransactionResult {
  slot?: number;
  blockTime?: number | null;
  transaction?: {
    signatures?: readonly string[];
    message?: {
      accountKeys?: readonly unknown[];
      instructions?: readonly { programIdIndex?: number }[];
    };
  };
  meta?: {
    err?: unknown;
    logMessages?: readonly string[];
    loadedAddresses?: { writable?: readonly unknown[]; readonly?: readonly unknown[] };
  } | null;
}

/** Pull the slots a rejected gRPC replay skipped, oldest first. Failed transactions are left out. */
export async function catchUpReplayGap(
  gap: ReplayGap,
  source: ReplayCatchUpSource,
  onTransaction: (tx: ParsedTargetTransaction) => void
): Promise<number> {
  const seen = new Set<string>();
  const found: ParsedTargetTransaction[] = [];
  for (const account of gap.accounts) {
    let before: string | undefined;
    for (let page = 0; page < MAX_PAGES; page += 1) {
      const signatures = await source.signatures(account, before);
      if (signatures.length === 0) break;
      let reached = false;
      for (const entry of signatures) {
        if (entry.slot < gap.fromSlot) {
          reached = true;
          break;
        }
        if (entry.err || seen.has(entry.signature)) continue;
        seen.add(entry.signature);
        const tx = await source.transaction(entry.signature);
        if (tx && tx.slot >= gap.fromSlot) found.push(tx);
      }
      if (reached || signatures.length < PAGE_SIZE) break;
      before = signatures.at(-1)?.signature;
      if (!before) break;
    }
  }
  found.sort((a, b) => a.slot - b.slot || a.signature.localeCompare(b.signature));
  for (const tx of found) onTransaction(tx);
  return found.length;
}

export function connectionReplaySource(connection: Connection): ReplayCatchUpSource {
  return {
    signatures: (account, before) => fetchSignatures(connection.rpcEndpoint, account, before),
    transaction: signature => fetchParsedTransaction(connection.rpcEndpoint, signature)
  };
}

async function fetchSignatures(rpcEndpoint: string, account: string, before?: string): Promise<readonly SignaturePageEntry[]> {
  const result = await rpc(rpcEndpoint, "getSignaturesForAddress", [account, { limit: PAGE_SIZE, before, commitment: "processed" }]);
  if (!Array.isArray(result)) return [];
  return result.flatMap(entry => {
    if (!entry || typeof entry !== "object") return [];
    const signature = "signature" in entry && typeof entry.signature === "string" ? entry.signature : undefined;
    const slot = "slot" in entry && typeof entry.slot === "number" ? entry.slot : undefined;
    if (!signature || slot === undefined) return [];
    return [{ signature, slot, err: "err" in entry ? entry.err : null }];
  });
}

async function rpc(rpcEndpoint: string, method: string, params: readonly unknown[]): Promise<unknown> {
  const response = await fetch(rpcEndpoint, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params })
  });
  if (!response.ok) throw new Error(`${method} ${response.status}`);
  const body = await response.json() as { error?: { message?: string }; result?: unknown };
  if (body.error) throw new Error(body.error.message ?? `${method} failed`);
  return body.result;
}

export async function fetchParsedTransaction(rpcEndpoint: string, signature: string): Promise<ParsedTargetTransaction | undefined> {
  const result = await rpc(rpcEndpoint, "getTransaction", [signature, { encoding: "json", commitment: "processed", maxSupportedTransactionVersion: 1 }]);
  return parsedTargetFromRpc(result as RpcTransactionResult | null | undefined);
}

export function parsedTargetFromRpc(result: RpcTransactionResult | null | undefined): ParsedTargetTransaction | undefined {
  const message = result?.transaction?.message;
  if (!result || result.meta?.err || !message) return undefined;
  const staticKeys = (message.accountKeys ?? []).map(keyString).filter((key): key is string => key !== undefined);
  const loaded = [...(result.meta?.loadedAddresses?.writable ?? []), ...(result.meta?.loadedAddresses?.readonly ?? [])].map(keyString).filter((key): key is string => key !== undefined);
  const accountKeys = [...staticKeys, ...loaded];
  const programIds = (message.instructions ?? []).map(ix => accountKeys[ix.programIdIndex ?? -1]).filter((key): key is string => key !== undefined);
  const signature = result.transaction?.signatures?.[0];
  if (!signature || result.slot === undefined) return undefined;
  return {
    signature,
    slot: result.slot,
    timestampMs: (result.blockTime ?? Math.floor(Date.now() / 1000)) * 1000,
    accountKeys,
    programIds,
    raw: { transaction: result.transaction, meta: result.meta }
  };
}

function keyString(key: unknown): string | undefined {
  if (typeof key === "string") return key;
  if (key && typeof key === "object" && "pubkey" in key && typeof key.pubkey === "string") return key.pubkey;
  return undefined;
}
