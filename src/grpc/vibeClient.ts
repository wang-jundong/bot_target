import type { ParsedTargetTransaction, PoolDescriptor } from "../venues/types.js";
import { createRequire } from "node:module";
import bs58 from "bs58";
import { CommitmentLevel, type SubscribeRequest, type SubscribeUpdate } from "@triton-one/yellowstone-grpc";

export interface VibeConnectionOptions { endpoint: string; token: string; }
export interface Subscription { close(): Promise<void>; }
export interface VibeClient {
  connect(): Promise<void>;
  subscribeWallet(wallet: string, onTransaction: (tx: ParsedTargetTransaction) => void): Promise<Subscription>;
  subscribePool(pool: PoolDescriptor, onTransaction: (tx: ParsedTargetTransaction) => void, fromSlot?: number): Promise<Subscription>;
  close(): Promise<void>;
}

const require = createRequire(import.meta.url);
interface Stream {
  on(event: "data", listener: (update: SubscribeUpdate) => void): this;
  on(event: "error", listener: (error: Error) => void): this;
  on(event: "end" | "close", listener: () => void): this;
  write(request: SubscribeRequest, callback: (error?: Error | null) => void): boolean;
  cancel(): void;
}
interface YellowstoneClientApi { ping(count: number): Promise<number>; subscribe(): Promise<Stream>; close?(): void; }
const YellowstoneClient = (require("@triton-one/yellowstone-grpc") as {
  default: new (endpoint: string, token: string | undefined, options: Record<string, number>) => YellowstoneClientApi;
}).default;

const RECONNECT_MS = 2_000;
const IDLE_MS = 300_000;

/** Replay slot Vibe will not serve. Retrying it on the next connect drops the stream again. */
function isReplayRejected(error: unknown): boolean {
  if (!error || typeof error !== "object") return false;
  const { code, message, details } = error as { code?: unknown; message?: unknown; details?: unknown };
  const text = [message, details].filter((part): part is string => typeof part === "string").join(" ");
  if (code === 11 || /\bOUT_OF_RANGE\b/.test(text)) return true;
  // Status 13. Vibe's replay backend failed; the shared stream is already dead.
  return /failed to get replay response/i.test(text);
}

export class YellowstoneVibeClient implements VibeClient {
  readonly #client: YellowstoneClientApi;
  readonly #handlers = new Map<string, Set<(tx: ParsedTargetTransaction) => void>>();
  readonly #retryTimers = new Set<NodeJS.Timeout>();
  #stream?: Stream;
  #replayStream?: Stream;
  #opening?: Promise<void>;
  #writeQueue: Promise<void> = Promise.resolve();
  #idleTimer?: NodeJS.Timeout;
  #closing = false;
  #lastSlot = 0;
  /** Next live filter write replays from this slot, then clears it. Ping writes do not. */
  #reconnectFromSlot?: number;
  /** True after this stream has accepted a filter. A later fromSlot has to be a new stream. */
  #filterWritten = false;
  /** Pool catch-up slot. Served on a side stream so a replay failure cannot drop wallet delivery. */
  #catchupSlot?: number;

  constructor(options: VibeConnectionOptions, private readonly onError: (error: unknown) => void = console.error, client?: YellowstoneClientApi) {
    this.#client = client ?? new YellowstoneClient(options.endpoint, options.token || undefined, {
      "grpc.max_receive_message_length": 16 * 1024 * 1024,
      "grpc.keepalive_time_ms": 20_000,
      "grpc.keepalive_timeout_ms": 10_000,
      "grpc.keepalive_permit_without_calls": 1,
      "grpc.http2.min_time_between_pings_ms": 10_000,
      "grpc.http2.max_pings_without_data": 0
    });
  }

  async connect(): Promise<void> {}
  subscribeWallet(wallet: string, onTransaction: (tx: ParsedTargetTransaction) => void): Promise<Subscription> {
    return this.#subscribe(wallet, onTransaction);
  }
  subscribePool(pool: PoolDescriptor, onTransaction: (tx: ParsedTargetTransaction) => void, fromSlot?: number): Promise<Subscription> {
    return this.#subscribe(pool.pool, onTransaction, fromSlot);
  }

  async #subscribe(account: string, onTransaction: (tx: ParsedTargetTransaction) => void, fromSlot?: number): Promise<Subscription> {
    let handlers = this.#handlers.get(account);
    if (!handlers) { handlers = new Set(); this.#handlers.set(account, handlers); }
    handlers.add(onTransaction);
    this.#armCatchup(fromSlot);
    while (!this.#closing) {
      try {
        await this.#ensureStream();
        await this.#writeSubscription();
        break;
      } catch (error) {
        this.onError(error);
        await new Promise<void>(resolve => setTimeout(resolve, RECONNECT_MS));
      }
    }
    if (!this.#closing && this.#catchupSlot) {
      try { await this.#openReplay(); }
      catch (error) { this.onError(error); }
    }
    if (this.#closing) {
      handlers.delete(onTransaction);
      if (handlers.size === 0) this.#handlers.delete(account);
      throw new Error("Vibe client closed while subscribing");
    }
    let closed = false;
    return {
      close: async () => {
        if (closed) return;
        closed = true;
        const current = this.#handlers.get(account);
        current?.delete(onTransaction);
        if (current?.size === 0) this.#handlers.delete(account);
        if (this.#stream && !this.#closing) await this.#writeSubscription();
      }
    };
  }

  #armReconnect(slot?: number): void {
    if (slot === undefined || !Number.isFinite(slot) || slot <= 0) return;
    if (this.#reconnectFromSlot === undefined || slot < this.#reconnectFromSlot) this.#reconnectFromSlot = slot;
  }

  #armCatchup(slot?: number): void {
    if (slot === undefined || !Number.isFinite(slot) || slot <= 0) return;
    if (this.#catchupSlot !== undefined && slot >= this.#catchupSlot) return;
    this.#catchupSlot = slot;
    if (this.#replayStream) this.#dropReplay();
  }

  #dropReplay(): void {
    const stream = this.#replayStream;
    this.#replayStream = undefined;
    stream?.cancel();
  }

  #request(fromSlot?: number, ping = false): SubscribeRequest {
    return {
      accounts: {}, slots: {}, transactionsStatus: {}, blocks: {}, blocksMeta: {}, entry: {},
      transactions: { tracked: { vote: false, failed: false, signature: undefined, accountInclude: [...this.#handlers.keys()], accountExclude: [], accountRequired: [] } },
      commitment: CommitmentLevel.PROCESSED,
      accountsDataSlice: [],
      ping: ping ? { id: 1 } : undefined,
      fromSlot: fromSlot && fromSlot > 0 ? String(fromSlot) : undefined
    };
  }

  async #ensureStream(): Promise<void> {
    if (this.#stream || this.#closing) return;
    if (!this.#opening) this.#opening = this.#open().finally(() => { this.#opening = undefined; });
    await this.#opening;
  }

  async #open(): Promise<void> {
    if (this.#closing) return;
    const stream = await this.#client.subscribe();
    this.#stream = stream;
    const disconnected = (error?: unknown): void => {
      if (!this.#detach(stream)) return;
      this.#dropReplay();
      if (isReplayRejected(error)) {
        // Vibe cannot serve this replay. Asking for the same slot again loops the stream.
        this.#reconnectFromSlot = undefined;
        this.#catchupSlot = undefined;
        this.#lastSlot = 0;
      } else {
        this.#armReconnect(this.#lastSlot);
      }
      if (error) this.onError(error);
      this.#scheduleReconnect();
    };
    stream.on("data", update => {
      if (update.ping) {
        void this.#writeSubscription(true).catch(error => this.onError(error));
        return;
      }
      this.#onData(update);
    });
    stream.on("error", error => disconnected(error));
    stream.on("end", () => disconnected(new Error("Vibe shared stream ended")));
    stream.on("close", () => disconnected(new Error("Vibe shared stream closed")));
    this.#armIdle();
    try { await this.#writeSubscription(); }
    catch (error) {
      if (this.#stream === stream) {
        this.#stream = undefined;
        this.#filterWritten = false;
      }
      this.#clearIdle();
      stream.cancel();
      throw error;
    }
  }

  /** Pool history on its own stream. A failure here leaves the live wallet stream up. */
  async #openReplay(): Promise<void> {
    const from = this.#catchupSlot;
    if (!from || this.#replayStream || this.#closing) return;
    const until = Math.max(this.#lastSlot, from);
    const stream = await this.#client.subscribe();
    if (this.#closing || this.#replayStream || this.#catchupSlot !== from) {
      stream.cancel();
      return;
    }
    this.#replayStream = stream;
    const stop = (error?: unknown): void => {
      if (this.#replayStream !== stream) return;
      this.#replayStream = undefined;
      this.#catchupSlot = undefined;
      stream.cancel();
      if (error && !this.#closing) this.onError(error);
    };
    stream.on("data", update => {
      if (update.ping) return;
      this.#onData(update);
      const slot = Number(update.transaction?.slot ?? 0);
      if (slot >= until) stop();
    });
    stream.on("error", error => stop(error));
    stream.on("end", () => stop(new Error("Vibe replay stream ended")));
    stream.on("close", () => stop());
    try {
      await this.#write(stream, this.#request(from));
    } catch (error) {
      stop(error);
    }
  }

  #onData(update: SubscribeUpdate): void {
    this.#armIdle();
    const info = update.transaction?.transaction;
    const message = info?.transaction?.message;
    if (!info || !message || info.meta?.err) return;
    const slot = Number(update.transaction!.slot);
    if (slot > this.#lastSlot) this.#lastSlot = slot;
    const accountKeys = [...message.accountKeys, ...(info.meta?.loadedWritableAddresses ?? []), ...(info.meta?.loadedReadonlyAddresses ?? [])].map(key => bs58.encode(key));
    const programIds = message.instructions.map(ix => accountKeys[ix.programIdIndex]).filter((key): key is string => key !== undefined);
    const transaction = { signature: bs58.encode(info.signature), slot, timestampMs: Date.now(), accountKeys, programIds, raw: info };
    for (const account of accountKeys) {
      const handler = this.#handlers.get(account)?.values().next().value;
      if (handler) { handler(transaction); break; }
    }
  }

  #writeSubscription(ping = false): Promise<void> {
    const run = this.#writeQueue.then(() => this.#writeOnce(ping));
    this.#writeQueue = run.then(() => undefined, () => undefined);
    return run;
  }

  async #writeOnce(ping: boolean): Promise<void> {
    const stream = this.#stream;
    if (!stream) return;
    const fromSlot = ping ? undefined : this.#reconnectFromSlot;
    if (!ping) this.#reconnectFromSlot = undefined;
    // Vibe returns 13 INTERNAL when fromSlot is written onto an open stream, and that kills every subscription.
    if (fromSlot && this.#filterWritten) {
      this.#armReconnect(fromSlot);
      this.#detach(stream);
      stream.cancel();
      this.#scheduleReconnect(0);
      return;
    }
    try {
      await this.#write(stream, this.#request(fromSlot, ping));
    } catch (error) {
      if (!fromSlot) throw error;
      if (isReplayRejected(error)) this.#lastSlot = 0;
      this.onError(error);
      await this.#write(stream, this.#request(undefined, ping));
    }
    if (!ping) this.#filterWritten = true;
  }

  #write(stream: Stream, request: SubscribeRequest): Promise<void> {
    return new Promise((resolve, reject) => stream.write(request, error => error ? reject(error) : resolve()));
  }

  #armIdle(): void {
    this.#clearIdle();
    this.#idleTimer = setTimeout(() => {
      if (this.#closing || !this.#stream) return;
      const stream = this.#stream;
      if (!this.#detach(stream)) return;
      this.#armReconnect(this.#lastSlot);
      stream.cancel();
      this.onError(new Error(`Vibe stream idle for ${IDLE_MS / 1000}s`));
      this.#scheduleReconnect();
    }, IDLE_MS);
  }

  #clearIdle(): void {
    if (!this.#idleTimer) return;
    clearTimeout(this.#idleTimer);
    this.#idleTimer = undefined;
  }

  #detach(stream: Stream): boolean {
    if (this.#stream !== stream) return false;
    this.#stream = undefined;
    this.#filterWritten = false;
    this.#clearIdle();
    return true;
  }

  #scheduleReconnect(delayMs = RECONNECT_MS): void {
    if (this.#closing || this.#handlers.size === 0 || this.#retryTimers.size > 0) return;
    const timer = setTimeout(() => {
      this.#retryTimers.delete(timer);
      void this.#ensureStream().catch(error => { this.onError(error); this.#scheduleReconnect(); });
    }, delayMs);
    this.#retryTimers.add(timer);
  }

  async close(): Promise<void> {
    this.#closing = true;
    for (const timer of this.#retryTimers) clearTimeout(timer);
    this.#retryTimers.clear();
    this.#clearIdle();
    this.#handlers.clear();
    this.#dropReplay();
    const stream = this.#stream;
    this.#stream = undefined;
    this.#filterWritten = false;
    stream?.cancel();
    this.#client.close?.();
  }
}
