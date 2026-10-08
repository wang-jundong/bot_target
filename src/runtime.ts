import type { Logger } from "pino";
import { PublicKey, type Connection } from "@solana/web3.js";
import type { AppConfig } from "./config/index.js";
import type { ReplayGap, Subscription, VibeClient } from "./grpc/vibeClient.js";
import { catchUpReplayGap, connectionReplaySource } from "./grpc/replayCatchUp.js";
import type { Strategy } from "./strategy/types.js";
import { TokenLifecycleState } from "./strategy/state.js";
import { poolKey, TokenStateManager } from "./state/tokenStateManager.js";
import type { TokenState } from "./state/tokenState.js";
import type { VenueAdapter } from "./venues/types.js";
import { PumpTradeDecoder } from "./venues/tradeDecoder.js";
import type { JournalRecord } from "./recovery/journal.js";
import type { RecoveryJournal } from "./recovery/journal.js";
import { allocateRecoveredTokens } from "./recovery/recoveredTokens.js";

/** Untagged records belong to the first active strategy, so one wallet bag is not restored twice. */
export function journalRecordMatchesStrategy(recordStrategy: string | undefined, slotName: string, activeNames: readonly string[]): boolean {
  if (recordStrategy) return recordStrategy === slotName;
  return activeNames[0] === slotName;
}

function walletMintKey(owner: PublicKey, mint: string): string {
  return `${owner.toBase58()}:${mint}`;
}

interface StrategySlot {
  name: string;
  strategy: Strategy;
  states: TokenStateManager;
  poolNeeded: Map<string, boolean>;
  buyAmountLamports: bigint;
  targetWallet: string;
  owner: PublicKey;
}

interface RecoveredOpen {
  slot: StrategySlot;
  record: JournalRecord;
  recordedAmount: bigint;
  entryPrice: number;
  entrySolAmount?: string;
  buySignature?: string;
  warnUntagged: boolean;
}

export class TradingRuntime {
  readonly #slots: StrategySlot[];
  readonly #owner = new WeakMap<TokenState, StrategySlot>();
  readonly #adapters: ReadonlyMap<string, VenueAdapter>;
  readonly #subscriptions = new Map<string, Subscription>();
  #queue: Promise<void> = Promise.resolve();
  #positionTimer?: NodeJS.Timeout;
  #healthTimer?: NodeJS.Timeout;
  #receivedTransactions = 0;
  #decodedEvents = 0;
  #closed = false;
  #catchUpQueue: Promise<void> = Promise.resolve();

  constructor(
    private readonly config: AppConfig,
    private readonly connection: Connection,
    private readonly vibe: VibeClient,
    private readonly journal: RecoveryJournal,
    private readonly decoder: PumpTradeDecoder,
    adapters: readonly VenueAdapter[],
    private readonly logger: Logger,
    strategies: readonly { name: string; strategy: Strategy; buyAmountLamports: bigint; targetWallet: string; owner: PublicKey }[]
  ) {
    this.#adapters = new Map(adapters.map(adapter => [adapter.name, adapter]));
    this.#slots = strategies.map(entry => ({
      name: entry.name,
      strategy: entry.strategy,
      states: new TokenStateManager(config.EVENT_RETENTION_SEC * 1000),
      poolNeeded: new Map(),
      buyAmountLamports: entry.buyAmountLamports,
      targetWallet: entry.targetWallet,
      owner: entry.owner
    }));
    for (const slot of this.#slots) {
      slot.strategy.setPoolTape?.((state, needed, fromSlot) => {
        const owner = this.#owner.get(state);
        if (!owner) return;
        owner.poolNeeded.set(poolKey(state.descriptor), needed);
        this.#queueSync(state.descriptor, needed ? fromSlot : undefined);
      });
    }
  }

  async start(): Promise<void> {
    this.logger.info("[02 STREAM] Connecting to Vibe gRPC");
    await this.vibe.connect();
    this.logger.info("[02 STREAM] Vibe client initialized");
    await this.#recoverPositions();
    const targetWallets = [...new Set(this.#slots.map(slot => slot.targetWallet))];
    for (const targetWallet of targetWallets) {
      this.logger.info({ targetWallet }, "[02 STREAM] Opening target-wallet subscription");
      const wallet = await this.vibe.subscribeWallet(targetWallet, tx => this.#enqueue(tx));
      this.#subscriptions.set(`wallet:${targetWallet}`, wallet);
      this.logger.info({ targetWallet }, "[02 STREAM] Target-wallet subscription active");
    }
    const tickMs = Math.max(50, Math.min(...this.#slots.map(slot => slot.strategy.timerMs || 200)));
    this.#positionTimer = setInterval(() => {
      this.#queue = this.#queue.then(async () => {
        const now = Date.now();
        for (const slot of this.#slots) {
          for (const state of [...slot.states.values()]) {
            if (this.#isTerminal(state)) {
              this.#dropState(slot, state, state.lifecycle === TokenLifecycleState.CLOSED ? "position closed" : "failed");
              continue;
            }
            slot.strategy.onClock(state, now);
            if (this.#isTerminal(state)) this.#dropState(slot, state, state.lifecycle === TokenLifecycleState.CLOSED ? "position closed" : "failed");
          }
        }
      }).catch(error => this.logger.error({ err: error instanceof Error ? error.message : String(error) }, "[ERROR] Position maintenance failed"));
    }, tickMs);
    this.#healthTimer = setInterval(() => {
      const strategies = this.#slots.map(slot => {
        const states = [...slot.states.values()];
        return {
          name: slot.name,
          trackedTokens: states.length,
          lifecycles: states.reduce<Record<string, number>>((counts, state) => { counts[state.lifecycle] = (counts[state.lifecycle] ?? 0) + 1; return counts; }, {})
        };
      });
      this.logger.info({
        receivedTransactions: this.#receivedTransactions,
        decodedEvents: this.#decodedEvents,
        subscriptions: this.#subscriptions.size,
        strategies
      }, "[HEALTH] Bot is running");
    }, 30_000);
    this.logger.info({ targets: this.#slots.map(slot => ({ strategy: slot.name, targetWallet: slot.targetWallet })), strategy: this.config.strategy, timerMs: tickMs }, "[01 STARTUP] Trading runtime started");
  }

  /**
   * Vibe drops a rejected replay instead of retrying it. The target's full sell often
   * lands in that hole, a few slots after the buy that opened the pool. Read it back
   * from RPC once the live stream is up, before the strategy can buy into an empty bag.
   */
  async recoverReplayGap(gap: ReplayGap): Promise<void> {
    if (this.#closed || gap.accounts.length === 0) return;
    const run = this.#catchUpQueue.then(async () => {
      if (this.#closed) return;
      await new Promise<void>(resolve => setTimeout(resolve, 500));
      if (this.#closed) return;
      const enqueued = await catchUpReplayGap(gap, connectionReplaySource(this.connection), tx => this.#enqueue(tx));
      this.logger.info({ fromSlot: gap.fromSlot, accounts: gap.accounts.length, enqueued }, "[02 STREAM] Caught up missed replay slots");
    });
    this.#catchUpQueue = run.then(() => undefined, () => undefined);
    return run;
  }

  #enqueue(tx: Parameters<PumpTradeDecoder["decode"]>[0]): void {
    this.#receivedTransactions++;
    this.logger.debug({ signature: tx.signature, slot: tx.slot, accountCount: tx.accountKeys.length, programCount: tx.programIds.length }, "[02 STREAM] Transaction received");
    this.#queue = this.#queue.then(() => this.#handle(tx)).catch(error => {
      this.logger.error({ err: error instanceof Error ? error.message : String(error), signature: tx.signature }, "[ERROR] Transaction processing failed");
    });
  }

  async #handle(tx: Parameters<PumpTradeDecoder["decode"]>[0]): Promise<void> {
    const decoded = await this.decoder.decode(tx);
    this.#decodedEvents += decoded.length;
    this.logger.debug({ signature: tx.signature, decodedEvents: decoded.length }, decoded.length ? "[03 DETECT] Pump trade decoded" : "[03 DETECT] Ignored non-Pump transaction");
    for (const parsed of decoded) {
      const { descriptor, event } = parsed;
      this.logger.debug({ signature: event.signature, mint: event.mint, pool: event.pool, venue: descriptor.venue, trader: event.trader, side: event.side, solAmount: event.solAmount, tokenAmount: event.tokenAmount, price: event.price, curveProgress: event.curveProgress }, "[03 DETECT] Trade details");
      const adapter = this.#adapters.get(descriptor.venue);
      if (!adapter) continue;
      if (event.curve) adapter.noteCurve?.(descriptor.mint, event.curve);
      if (event.swap) adapter.noteSwap?.(descriptor.mint, event.swap);
      const created: string[] = [];
      for (const slot of this.#slots) if (this.#apply(slot, descriptor, adapter, event)) created.push(slot.name);
      if (created.length) this.logger.info({ mint: descriptor.mint, pool: descriptor.pool, venue: descriptor.venue, signature: event.signature, strategies: created }, "[03 DETECT] Target bought token; now tracking pool");
    }
  }

  #apply(slot: StrategySlot, descriptor: Parameters<VibeClient["subscribePool"]>[0], adapter: VenueAdapter, event: Parameters<Strategy["onEvent"]>[1]): boolean {
    let state = slot.states.get(descriptor);
    if (!state && event.trader === slot.targetWallet && event.side === "buy") {
      state = slot.states.create(descriptor, adapter, event);
      this.#owner.set(state, slot);
      slot.poolNeeded.set(poolKey(descriptor), true);
      state.transition(TokenLifecycleState.TRACKING_POOL);
      state.prices.currentMarkPrice = event.price;
      slot.strategy.onEvent(state, event);
      if (this.#isTerminal(state)) this.#dropState(slot, state, "strategy finished");
      else this.#queueSync(descriptor, event.slot);
      return true;
    }
    if (!state) return false;
    if (event.trader === slot.targetWallet) state.recordTargetTrade(event);
    if (!state.events.add(event)) return false;
    state.prices.currentMarkPrice = event.price;
    slot.strategy.onEvent(state, event);
    if (this.#isTerminal(state)) this.#dropState(slot, state, "strategy finished");
    return false;
  }

  async #recoverPositions(): Promise<void> {
    const records = await this.journal.read();
    const activeNames = this.#slots.map(slot => slot.name);
    const opens: RecoveredOpen[] = [];
    for (const slot of this.#slots) opens.push(...await this.#openRecords(slot, records, activeNames));
    const walletByMint = new Map<string, bigint>();
    for (const open of opens) {
      const mint = open.record.descriptor?.mint;
      const key = mint ? walletMintKey(open.slot.owner, mint) : "";
      if (!mint || walletByMint.has(key)) continue;
      walletByMint.set(key, await this.#walletTokenAmount(open.slot.owner, mint));
    }
    for (const open of opens) {
      const descriptor = open.record.descriptor;
      if (!descriptor) continue;
      const others = opens.filter(candidate => candidate !== open && candidate.record.descriptor?.mint === descriptor.mint && candidate.slot.owner.equals(open.slot.owner));
      const tokenAmount = allocateRecoveredTokens(
        open.recordedAmount,
        walletByMint.get(walletMintKey(open.slot.owner, descriptor.mint)) ?? 0n,
        others.reduce((sum, candidate) => sum + candidate.recordedAmount, 0n),
        others.filter(candidate => candidate.recordedAmount === 0n).length
      );
      if (tokenAmount <= 0n || open.entryPrice <= 0) {
        if (others.length > 0) this.logger.error({ mint: descriptor.mint, strategy: open.slot.name, recordedAmount: open.recordedAmount.toString() }, "[07 POSITION] Skipped recovery; strategy token amount could not be separated");
        continue;
      }
      await this.#restoreOpen(open, tokenAmount);
    }
  }

  async #openRecords(slot: StrategySlot, records: readonly JournalRecord[], activeNames: readonly string[]): Promise<RecoveredOpen[]> {
    const latest = new Map<string, JournalRecord>();
    const lastBuyFillLamports = new Map<string, string>();
    const lastBuySignature = new Map<string, string>();
    for (const record of records) {
      if (!record.descriptor) continue;
      if (!journalRecordMatchesStrategy(record.strategy, slot.name, activeNames)) continue;
      const key = poolKey(record.descriptor);
      latest.set(key, record);
      if (record.event === "position_closed") {
        lastBuyFillLamports.delete(key);
        lastBuySignature.delete(key);
      } else {
        if (record.event === "buy_processed" && record.fill?.solAmount) lastBuyFillLamports.set(key, record.fill.solAmount);
        if (record.event === "buy_sent" && record.signature) lastBuySignature.set(key, record.signature);
      }
    }
    const opens: RecoveredOpen[] = [];
    for (const record of latest.values()) {
      const descriptor = record.descriptor;
      if (!descriptor || record.lifecycle === TokenLifecycleState.CLOSED || record.lifecycle === TokenLifecycleState.FAILED) continue;
      const adapter = this.#adapters.get(descriptor.venue);
      if (!adapter) continue;
      let recordedAmount = record.actualTokenAmount ? BigInt(record.actualTokenAmount) : 0n;
      let entryPrice = record.prices?.actualEntryFillPrice ?? 0;
      if (recordedAmount === 0n && record.signature) {
        const tx = await this.connection.getTransaction(record.signature, { commitment: "confirmed", maxSupportedTransactionVersion: 0 });
        if (tx && !tx.meta?.err) {
          const fill = adapter.parseFill(tx, slot.owner, descriptor.mint);
          recordedAmount = fill.tokenAmount;
          if (entryPrice <= 0) entryPrice = fill.price;
        }
      }
      const key = poolKey(descriptor);
      opens.push({
        slot,
        record,
        recordedAmount,
        entryPrice,
        entrySolAmount: record.actualEntrySolAmount ?? lastBuyFillLamports.get(key),
        buySignature: record.buySignature ?? lastBuySignature.get(key) ?? record.signature,
        warnUntagged: activeNames.length > 1 && !record.strategy
      });
    }
    return opens;
  }

  async #walletTokenAmount(owner: PublicKey, mint: string): Promise<bigint> {
    const accounts = await this.connection.getParsedTokenAccountsByOwner(owner, { mint: new PublicKey(mint) }, "confirmed");
    return accounts.value.reduce((sum, account) => sum + BigInt(account.account.data.parsed.info.tokenAmount.amount as string), 0n);
  }

  async #restoreOpen(open: RecoveredOpen, tokenAmount: bigint): Promise<void> {
    const descriptor = open.record.descriptor;
    if (!descriptor) return;
    const adapter = this.#adapters.get(descriptor.venue);
    if (!adapter) return;
    const now = Date.now();
    const seedEvent = {
      signature: open.record.signature ?? "recovered",
      slot: 0,
      eventIndex: 0,
      timestampMs: open.record.entryProcessedMs ?? now,
      receivedMonoMs: performance.now(),
      mint: descriptor.mint,
      pool: descriptor.pool,
      programId: descriptor.programId,
      trader: open.slot.targetWallet,
      side: "buy" as const,
      solAmount: 0n,
      tokenAmount,
      price: open.entryPrice,
      curveProgress: descriptor.venue === "pumpswap" ? 1 : undefined
    };
    const state = open.slot.states.create(descriptor, adapter, seedEvent);
    this.#owner.set(state, open.slot);
    open.slot.poolNeeded.set(poolKey(descriptor), true);
    state.restorePosition(tokenAmount, open.entryPrice, open.record.prices?.currentMarkPrice ?? open.entryPrice, open.record.entryProcessedMs ?? now);
    state.buyLamports = open.record.buyLamports ? BigInt(open.record.buyLamports) : open.slot.buyAmountLamports;
    state.actualEntrySolAmount = open.entrySolAmount ? BigInt(open.entrySolAmount) : undefined;
    state.buySignature = open.buySignature;
    await this.#subscribePool(descriptor);
    open.slot.strategy.restoreOpenPosition?.(state, open.entryPrice);
    if (open.warnUntagged) this.logger.warn({ mint: descriptor.mint, strategy: open.slot.name }, "[07 POSITION] Untagged position restored into the first strategy");
    this.logger.warn({ mint: descriptor.mint, pool: descriptor.pool, tokenAmount, strategy: open.slot.name }, "[07 POSITION] Recovered existing position");
  }

  canOpenPosition(candidate: TokenState): boolean {
    const blocked = new Set(this.config.BLOCKED_MINTS.split(",").map(value => value.trim()).filter(Boolean));
    if (blocked.has(candidate.descriptor.mint)) { this.logger.warn({ mint: candidate.descriptor.mint }, "[05 ENTRY] Buy blocked: mint denylist"); return false; }
    return true;
  }

  #isTerminal(state: TokenState): boolean {
    return state.lifecycle === TokenLifecycleState.CLOSED || state.lifecycle === TokenLifecycleState.FAILED;
  }

  #dropState(slot: StrategySlot, state: TokenState, reason: string): void {
    const descriptor = state.descriptor;
    slot.poolNeeded.delete(poolKey(descriptor));
    slot.states.delete(descriptor);
    this.logger.info({ mint: descriptor.mint, pool: descriptor.pool, strategy: slot.name, reason }, "[CLEANUP] Strategy stopped tracking mint");
    this.#queueSync(descriptor);
  }

  #queueSync(descriptor: Parameters<VibeClient["subscribePool"]>[0], fromSlot?: number): void {
    this.#queue = this.#queue.then(() => this.#syncSubscription(descriptor, fromSlot)).catch(error => {
      this.logger.error({ err: error instanceof Error ? error.message : String(error), mint: descriptor.mint }, "[02 STREAM] Pool filter update failed");
    });
  }

  async #syncSubscription(descriptor: Parameters<VibeClient["subscribePool"]>[0], fromSlot?: number): Promise<void> {
    const key = poolKey(descriptor);
    const wanted = this.#slots.some(slot => slot.poolNeeded.get(key) === true);
    if (wanted) await this.#subscribePool(descriptor, fromSlot);
    else await this.#releaseSubscription(key, descriptor);
  }

  async #subscribePool(descriptor: Parameters<VibeClient["subscribePool"]>[0], fromSlot?: number): Promise<void> {
    const key = `${descriptor.programId}:${descriptor.pool}`;
    if (this.#subscriptions.has(key)) return;
    this.logger.info({ mint: descriptor.mint, pool: descriptor.pool, venue: descriptor.venue, fromSlot }, "[02 STREAM] Opening pool subscription");
    const subscription = await this.vibe.subscribePool(descriptor, tx => this.#enqueue(tx), fromSlot);
    this.#subscriptions.set(key, subscription);
    this.logger.info({ mint: descriptor.mint, pool: descriptor.pool, subscriptions: this.#subscriptions.size }, "[02 STREAM] Pool subscription active");
  }

  async #releaseSubscription(key: string, descriptor: { mint: string; pool: string }): Promise<void> {
    const subscription = this.#subscriptions.get(key);
    if (!subscription) return;
    await subscription.close();
    this.#subscriptions.delete(key);
    this.logger.info({ mint: descriptor.mint, pool: descriptor.pool, subscriptions: this.#subscriptions.size }, "[02 STREAM] Released pool from gRPC filter");
  }

  async close(): Promise<void> {
    this.#closed = true;
    const tracked = this.#slots.reduce((sum, slot) => sum + [...slot.states.values()].length, 0);
    this.logger.info({ subscriptions: this.#subscriptions.size, receivedTransactions: this.#receivedTransactions, decodedEvents: this.#decodedEvents, tracked }, "[SHUTDOWN] Trading runtime stopping");
    if (this.#positionTimer) clearInterval(this.#positionTimer);
    if (this.#healthTimer) clearInterval(this.#healthTimer);
    await Promise.allSettled([...this.#subscriptions.values()].map(subscription => subscription.close()));
    this.#subscriptions.clear();
    await this.vibe.close();
  }
}
