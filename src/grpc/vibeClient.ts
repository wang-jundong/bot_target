import type { ParsedTargetTransaction, PoolDescriptor } from "../venues/types.js";
import { createRequire } from "node:module";
import bs58 from "bs58";
import { CommitmentLevel, type ReconnectEvent, type SubscribeRequest, type SubscribeUpdate } from "@triton-one/yellowstone-grpc";

export interface VibeConnectionOptions { endpoint: string; token: string; }
export interface Subscription { close(): Promise<void>; }
export interface VibeClient {
  connect(): Promise<void>;
  subscribeWallet(wallet: string, onTransaction: (tx: ParsedTargetTransaction) => void): Promise<Subscription>;
  subscribePool(pool: PoolDescriptor, onTransaction: (tx: ParsedTargetTransaction) => void): Promise<Subscription>;
  close(): Promise<void>;
}

const require = createRequire(import.meta.url);
interface Stream {
  on(event: "data", listener: (event: ReconnectEvent) => void): this;
  on(event: "error", listener: (error: Error) => void): this;
  on(event: "end" | "close", listener: () => void): this;
  write(request: SubscribeRequest, callback: (error?: Error | null) => void): boolean;
  cancel(): void;
}
interface YellowstoneClientApi {
  connect(): Promise<void>;
  subscribeWithReconnect(request?: SubscribeRequest): Promise<Stream>;
  close?(): void;
}
const YellowstoneClient = (require("@triton-one/yellowstone-grpc") as {
  default: new (
    endpoint: string,
    token: string | undefined,
    options: {
      grpcMaxDecodingMessageSize?: number;
      grpcHttp2KeepAliveInterval?: number;
      grpcKeepAliveTimeout?: number;
      grpcKeepAliveWhileIdle?: boolean;
    },
    reconnect?: { backoff?: { initialIntervalMs?: number; multiplier?: number; maxRetries?: number } }
  ) => YellowstoneClientApi;
}).default;

const IDLE_MS = 300_000;

export class YellowstoneVibeClient implements VibeClient {
  readonly #client: YellowstoneClientApi;
  readonly #handlers = new Map<string, Set<(tx: ParsedTargetTransaction) => void>>();
  readonly #retryTimers = new Set<NodeJS.Timeout>();
  #stream?: Stream;
  #opening?: Promise<void>;
  #writeQueue: Promise<void> = Promise.resolve();
  #idleTimer?: NodeJS.Timeout;
  #closing = false;

  constructor(options: VibeConnectionOptions, private readonly onError: (error: unknown) => void = console.error, client?: YellowstoneClientApi) {
    this.#client = client ?? new YellowstoneClient(options.endpoint, options.token || undefined, {
      grpcMaxDecodingMessageSize: 16 * 1024 * 1024,
      grpcHttp2KeepAliveInterval: 20_000,
      grpcKeepAliveTimeout: 10_000,
      grpcKeepAliveWhileIdle: true
    }, {
      backoff: { initialIntervalMs: 100, multiplier: 2, maxRetries: 10 }
    });
  }

  async connect(): Promise<void> {
    await this.#client.connect();
  }
  subscribeWallet(wallet: string, onTransaction: (tx: ParsedTargetTransaction) => void): Promise<Subscription> {
    return this.#subscribe(wallet, onTransaction);
  }
  subscribePool(pool: PoolDescriptor, onTransaction: (tx: ParsedTargetTransaction) => void): Promise<Subscription> {
    return this.#subscribe(pool.pool, onTransaction);
  }

  async #subscribe(account: string, onTransaction: (tx: ParsedTargetTransaction) => void): Promise<Subscription> {
    let handlers = this.#handlers.get(account);
    if (!handlers) { handlers = new Set(); this.#handlers.set(account, handlers); }
    handlers.add(onTransaction);
    while (!this.#closing) {
      try {
        await this.#ensureStream();
        await this.#writeSubscription();
        break;
      } catch (error) {
        this.onError(error);
        await new Promise<void>(resolve => setTimeout(resolve, 0));
      }
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

  /** Live filter only. fromSlot asks Vibe to replay, and that replay returns status 13. */
  #request(ping = false): SubscribeRequest {
    return {
      accounts: {}, slots: {}, transactionsStatus: {}, blocks: {}, blocksMeta: {}, blockFooter: {}, entry: {},
      transactions: { tracked: { vote: false, failed: false, signature: undefined, accountInclude: [...this.#handlers.keys()], accountExclude: [], accountRequired: [] } },
      commitment: CommitmentLevel.PROCESSED,
      accountsDataSlice: [],
      ping: ping ? { id: 1 } : undefined
    };
  }

  async #ensureStream(): Promise<void> {
    if (this.#stream || this.#closing) return;
    if (!this.#opening) this.#opening = this.#open().finally(() => { this.#opening = undefined; });
    await this.#opening;
  }

  async #open(): Promise<void> {
    if (this.#closing) return;
    const stream = await this.#client.subscribeWithReconnect();
    this.#stream = stream;
    const disconnected = (error?: unknown): void => {
      if (!this.#detach(stream)) return;
      if (error) this.onError(error);
      this.#scheduleReconnect();
    };
    stream.on("data", event => {
      this.#armIdle();
      if (event.type !== "Update") return;
      const update: SubscribeUpdate = event.update;
      if (update.ping) {
        void this.#writeSubscription(true).catch(error => this.onError(error));
        return;
      }
      const info = update.transaction?.transaction;
      const message = info?.transaction?.message;
      if (!info || !message || info.meta?.err) return;
      const slot = Number(update.transaction!.slot);
      const accountKeys = [...message.accountKeys, ...(info.meta?.loadedWritableAddresses ?? []), ...(info.meta?.loadedReadonlyAddresses ?? [])].map(key => bs58.encode(key));
      const programIds = message.instructions.map(ix => accountKeys[ix.programIdIndex]).filter((key): key is string => key !== undefined);
      const transaction = { signature: bs58.encode(info.signature), slot, timestampMs: Date.now(), accountKeys, programIds, raw: info };
      this.#deliver(transaction);
    });
    stream.on("error", error => disconnected(error));
    stream.on("end", () => disconnected(new Error("Vibe shared stream ended")));
    stream.on("close", () => disconnected(new Error("Vibe shared stream closed")));
    this.#armIdle();
    try { await this.#writeSubscription(); }
    catch (error) {
      if (this.#stream === stream) this.#stream = undefined;
      this.#clearIdle();
      stream.cancel();
      throw error;
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
    await this.#write(stream, this.#request(ping));
  }

  #deliver(tx: ParsedTargetTransaction): void {
    for (const account of tx.accountKeys) {
      const handler = this.#handlers.get(account)?.values().next().value;
      if (handler) { handler(tx); break; }
    }
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
    this.#clearIdle();
    return true;
  }

  #scheduleReconnect(): void {
    if (this.#closing || this.#handlers.size === 0 || this.#retryTimers.size > 0) return;
    const timer = setTimeout(() => {
      this.#retryTimers.delete(timer);
      void this.#ensureStream().catch(error => { this.onError(error); this.#scheduleReconnect(); });
    }, 0);
    this.#retryTimers.add(timer);
  }

  async close(): Promise<void> {
    this.#closing = true;
    for (const timer of this.#retryTimers) clearTimeout(timer);
    this.#retryTimers.clear();
    this.#clearIdle();
    this.#handlers.clear();
    this.#stream?.cancel();
    this.#stream = undefined;
    this.#client.close?.();
  }
}
