import type { ParsedTargetTransaction, PoolDescriptor } from "../venues/types.js";
import bs58 from "bs58";
import Yellowstone, { CommitmentLevel, type ReconnectEvent, type SubscribeRequest, type SubscribeUpdate } from "@triton-one/yellowstone-grpc";

type YellowstoneClient = typeof import("@triton-one/yellowstone-grpc").default;
// The ESM build exports the class as default. TypeScript types the package as CommonJS, so the import needs this cast.
const Client = Yellowstone as unknown as YellowstoneClient;

export interface VibeConnectionOptions { endpoint: string; token: string; }
export interface Subscription { close(): Promise<void>; }
export interface VibeClient {
  connect(): Promise<void>;
  subscribeWallet(wallet: string, onTransaction: (tx: ParsedTargetTransaction) => void): Promise<Subscription>;
  subscribePool(pool: PoolDescriptor, onTransaction: (tx: ParsedTargetTransaction) => void): Promise<Subscription>;
  close(): Promise<void>;
}

interface Stream {
  on(event: "data", listener: (event: ReconnectEvent) => void): this;
  on(event: "error", listener: (error: Error) => void): this;
  on(event: "end" | "close", listener: () => void): this;
  write(request: SubscribeRequest, callback: (error?: Error | null) => void): boolean;
  destroy(error?: Error): this;
}
interface YellowstoneClientApi {
  connect(): Promise<void>;
  subscribeWithReconnect(request?: SubscribeRequest): Promise<Stream>;
}

const IDLE_MS = 300_000;

export class YellowstoneVibeClient implements VibeClient {
  readonly #client: YellowstoneClientApi;
  readonly #handlers = new Map<string, Set<(tx: ParsedTargetTransaction) => void>>();
  readonly #streams = new Map<string, Stream>();
  readonly #opening = new Map<string, Promise<void>>();
  readonly #retryTimers = new Map<string, NodeJS.Timeout>();
  readonly #idleTimers = new Map<string, NodeJS.Timeout>();
  #writeQueue: Promise<void> = Promise.resolve();
  #closing = false;

  constructor(options: VibeConnectionOptions, private readonly onError: (error: unknown) => void = console.error, client?: YellowstoneClientApi) {
    this.#client = client ?? new Client(options.endpoint, options.token || undefined, {
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
        await this.#ensureStream(account);
        break;
      } catch (error) {
        this.onError(error);
        await new Promise<void>(resolve => setTimeout(resolve, 0));
      }
    }
    if (this.#closing || !this.#streams.has(account)) {
      this.#dropHandler(account, onTransaction);
      throw new Error("Vibe client closed while subscribing");
    }
    let closed = false;
    return {
      close: async () => {
        if (closed) return;
        closed = true;
        this.#dropHandler(account, onTransaction);
        if (this.#handlers.has(account)) return;
        const stream = this.#streams.get(account);
        if (stream && this.#detach(account, stream)) stream.destroy();
      }
    };
  }

  /** Version 9 locks this filter at open. A later write may only be a ping, and fromSlot makes Vibe reply with status 13. */
  #request(account: string): SubscribeRequest {
    return {
      accounts: {}, slots: {}, transactionsStatus: {}, blocks: {}, blocksMeta: {}, blockFooter: {}, entry: {},
      transactions: { tracked: { vote: false, failed: false, signature: undefined, accountInclude: [account], accountExclude: [], accountRequired: [] } },
      commitment: CommitmentLevel.PROCESSED,
      accountsDataSlice: []
    };
  }

  #pingRequest(): SubscribeRequest {
    return {
      accounts: {}, slots: {}, transactions: {}, transactionsStatus: {}, blocks: {}, blocksMeta: {}, blockFooter: {}, entry: {},
      commitment: CommitmentLevel.PROCESSED,
      accountsDataSlice: [],
      ping: { id: 1 }
    };
  }

  async #ensureStream(account: string): Promise<void> {
    if (this.#streams.has(account) || this.#closing) return;
    let opening = this.#opening.get(account);
    if (!opening) {
      opening = this.#open(account).finally(() => { this.#opening.delete(account); });
      this.#opening.set(account, opening);
    }
    await opening;
  }

  async #open(account: string): Promise<void> {
    if (this.#closing || !this.#handlers.has(account)) return;
    const stream = await this.#client.subscribeWithReconnect(this.#request(account));
    if (this.#closing || !this.#handlers.has(account) || this.#streams.has(account)) {
      stream.destroy();
      return;
    }
    this.#streams.set(account, stream);
    const disconnected = (error?: unknown): void => {
      if (!this.#detach(account, stream)) return;
      if (error) this.onError(error);
      this.#scheduleReconnect(account);
    };
    stream.on("data", event => {
      this.#armIdle(account);
      if (event.type === "DiscardBanks") return;
      if (event.type !== "Update") return;
      const update: SubscribeUpdate = event.update;
      if (update.ping) {
        this.#ping(stream);
        return;
      }
      const info = update.transaction?.transaction;
      const message = info?.transaction?.message;
      if (!info || !message || info.meta?.err) return;
      const slot = Number(update.transaction!.slot);
      const accountKeys = [...message.accountKeys, ...(info.meta?.loadedWritableAddresses ?? []), ...(info.meta?.loadedReadonlyAddresses ?? [])].map(key => bs58.encode(key));
      const programIds = message.instructions.map(ix => accountKeys[ix.programIdIndex]).filter((key): key is string => key !== undefined);
      this.#deliver(account, { signature: bs58.encode(info.signature), slot, timestampMs: Date.now(), accountKeys, programIds, raw: info });
    });
    stream.on("error", error => disconnected(error));
    stream.on("end", () => disconnected(new Error("Vibe stream ended")));
    stream.on("close", () => disconnected(new Error("Vibe stream closed")));
    this.#armIdle(account);
  }

  #ping(stream: Stream): void {
    const run = this.#writeQueue.then(() => this.#write(stream, this.#pingRequest()));
    this.#writeQueue = run.then(() => undefined, () => undefined);
    void run.catch(error => this.onError(error));
  }

  #deliver(account: string, tx: ParsedTargetTransaction): void {
    this.#handlers.get(account)?.values().next().value?.(tx);
  }

  #write(stream: Stream, request: SubscribeRequest): Promise<void> {
    return new Promise((resolve, reject) => stream.write(request, error => error ? reject(error) : resolve()));
  }

  #armIdle(account: string): void {
    this.#clearIdle(account);
    this.#idleTimers.set(account, setTimeout(() => {
      this.#idleTimers.delete(account);
      const stream = this.#streams.get(account);
      if (this.#closing || !stream || !this.#detach(account, stream)) return;
      stream.destroy();
      this.onError(new Error(`Vibe stream idle for ${IDLE_MS / 1000}s`));
      this.#scheduleReconnect(account);
    }, IDLE_MS));
  }

  #clearIdle(account: string): void {
    const timer = this.#idleTimers.get(account);
    if (!timer) return;
    clearTimeout(timer);
    this.#idleTimers.delete(account);
  }

  #detach(account: string, stream: Stream): boolean {
    if (this.#streams.get(account) !== stream) return false;
    this.#streams.delete(account);
    this.#clearIdle(account);
    return true;
  }

  #dropHandler(account: string, onTransaction: (tx: ParsedTargetTransaction) => void): void {
    const handlers = this.#handlers.get(account);
    handlers?.delete(onTransaction);
    if (!handlers || handlers.size === 0) this.#handlers.delete(account);
  }

  #scheduleReconnect(account: string): void {
    if (this.#closing || !this.#handlers.has(account) || this.#retryTimers.has(account)) return;
    const timer = setTimeout(() => {
      this.#retryTimers.delete(account);
      void this.#ensureStream(account).catch(error => { this.onError(error); this.#scheduleReconnect(account); });
    }, 0);
    this.#retryTimers.set(account, timer);
  }

  async close(): Promise<void> {
    this.#closing = true;
    for (const timer of this.#retryTimers.values()) clearTimeout(timer);
    this.#retryTimers.clear();
    for (const timer of this.#idleTimers.values()) clearTimeout(timer);
    this.#idleTimers.clear();
    this.#handlers.clear();
    const streams = [...this.#streams.values()];
    this.#streams.clear();
    for (const stream of streams) stream.destroy();
  }
}
