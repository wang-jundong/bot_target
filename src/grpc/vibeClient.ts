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
  #stream?: Stream;
  #openKey = "";
  #pending?: Promise<void>;
  #retryTimer?: NodeJS.Timeout;
  #idleTimer?: NodeJS.Timeout;
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
        await this.#whenSynced();
        break;
      } catch (error) {
        this.onError(error);
        await new Promise<void>(resolve => setTimeout(resolve, 0));
      }
    }
    if (this.#closing || !this.#stream) {
      this.#dropHandler(account, onTransaction);
      throw new Error("Vibe client closed while subscribing");
    }
    let closed = false;
    return {
      close: async () => {
        if (closed) return;
        closed = true;
        this.#dropHandler(account, onTransaction);
        if (this.#closing) return;
        await this.#whenSynced();
      }
    };
  }

  /** Version 9 locks the filter at open. One stream carries every account, and a changed set replaces that stream. A later write is only a ping. */
  #request(accounts: readonly string[]): SubscribeRequest {
    return {
      accounts: {}, slots: {}, transactionsStatus: {}, blocks: {}, blocksMeta: {}, blockFooter: {}, entry: {},
      transactions: { tracked: { vote: false, failed: false, signature: undefined, accountInclude: [...accounts], accountExclude: [], accountRequired: [] } },
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

  #accounts(): string[] {
    return [...this.#handlers.keys()].sort();
  }

  #accountKey(): string {
    return this.#accounts().join("\0");
  }

  #synced(): boolean {
    const key = this.#accountKey();
    return key === this.#openKey && (key === "" || this.#stream !== undefined);
  }

  async #whenSynced(): Promise<void> {
    while (!this.#closing && !this.#synced()) await this.#kick();
  }

  #kick(): Promise<void> {
    if (this.#pending) return this.#pending;
    if (this.#closing || this.#synced()) return Promise.resolve();
    const run = this.#apply();
    this.#pending = run;
    void run.finally(() => {
      if (this.#pending === run) this.#pending = undefined;
    }).then(() => {
      if (!this.#closing && !this.#synced()) this.#kick();
    }, () => undefined);
    return run;
  }

  async #apply(): Promise<void> {
    while (!this.#closing) {
      const accounts = this.#accounts();
      const key = accounts.join("\0");
      if (key === this.#openKey && (key === "" || this.#stream)) return;
      this.#dropStream();
      if (key === "") return;
      const stream = await this.#client.subscribeWithReconnect(this.#request(accounts));
      if (this.#closing || this.#accountKey() !== key) {
        stream.destroy();
        continue;
      }
      this.#attach(stream, key);
      return;
    }
  }

  #attach(stream: Stream, key: string): void {
    this.#stream = stream;
    this.#openKey = key;
    let dropped = false;
    const disconnected = (error?: unknown): void => {
      if (dropped || this.#stream !== stream) return;
      dropped = true;
      this.#stream = undefined;
      this.#openKey = "";
      this.#clearIdle();
      if (error) this.onError(error);
      this.#scheduleRetry();
    };
    stream.on("data", event => {
      if (this.#stream !== stream) return;
      this.#armIdle();
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
      const programIds = message.instructions.map(ix => accountKeys[ix.programIdIndex]).filter((id): id is string => id !== undefined);
      this.#deliver({ signature: bs58.encode(info.signature), slot, timestampMs: Date.now(), accountKeys, programIds, raw: info });
    });
    stream.on("error", error => disconnected(error));
    stream.on("end", () => disconnected(new Error("Vibe stream ended")));
    stream.on("close", () => disconnected(new Error("Vibe stream closed")));
    this.#armIdle();
  }

  #ping(stream: Stream): void {
    const run = this.#writeQueue.then(() => this.#write(stream, this.#pingRequest()));
    this.#writeQueue = run.then(() => undefined, () => undefined);
    void run.catch(error => this.onError(error));
  }

  #deliver(tx: ParsedTargetTransaction): void {
    const present = new Set(tx.accountKeys);
    for (const [account, handlers] of this.#handlers) {
      if (!present.has(account)) continue;
      handlers.values().next().value?.(tx);
    }
  }

  #write(stream: Stream, request: SubscribeRequest): Promise<void> {
    return new Promise((resolve, reject) => stream.write(request, error => error ? reject(error) : resolve()));
  }

  #armIdle(): void {
    this.#clearIdle();
    this.#idleTimer = setTimeout(() => {
      this.#idleTimer = undefined;
      if (this.#closing || !this.#stream) return;
      this.#dropStream();
      this.onError(new Error(`Vibe stream idle for ${IDLE_MS / 1000}s`));
      this.#scheduleRetry();
    }, IDLE_MS);
  }

  #clearIdle(): void {
    if (!this.#idleTimer) return;
    clearTimeout(this.#idleTimer);
    this.#idleTimer = undefined;
  }

  #dropStream(): void {
    const stream = this.#stream;
    this.#stream = undefined;
    this.#openKey = "";
    this.#clearIdle();
    stream?.destroy();
  }

  #dropHandler(account: string, onTransaction: (tx: ParsedTargetTransaction) => void): void {
    const handlers = this.#handlers.get(account);
    handlers?.delete(onTransaction);
    if (!handlers || handlers.size === 0) this.#handlers.delete(account);
  }

  #scheduleRetry(): void {
    if (this.#closing || this.#handlers.size === 0 || this.#retryTimer) return;
    this.#retryTimer = setTimeout(() => {
      this.#retryTimer = undefined;
      void this.#kick().catch(error => { this.onError(error); this.#scheduleRetry(); });
    }, 0);
  }

  async close(): Promise<void> {
    this.#closing = true;
    if (this.#retryTimer) clearTimeout(this.#retryTimer);
    this.#retryTimer = undefined;
    this.#handlers.clear();
    this.#dropStream();
  }
}
