import type { Connection, Keypair, VersionedTransaction } from "@solana/web3.js";
import type { Logger } from "pino";
import type { Strategy, StrategyExecution } from "../strategy/types.js";
import { entryMarketCapSol, isEntryMarketCapAllowed } from "./entryGuards.js";
import { TokenLifecycleState } from "../strategy/state.js";
import type { TokenState } from "../state/tokenState.js";
import type { BlockhashManager } from "./blockhashManager.js";
import { ConfirmationTimeoutError, type ConfirmationTracker } from "./confirmationTracker.js";
import { HeliusSenderUnavailableError, type HeliusSender, type SenderTiming } from "../helius/sender.js";
import type { RecoveryJournal } from "../recovery/journal.js";
import { isNonRetryableBuyError } from "./buyRetryPolicy.js";
import type { PnlJournal } from "../pnl/pnlJournal.js";
import { MintTradeLock } from "./mintTradeLock.js";

export class LiveStrategyExecution implements StrategyExecution {
  #strategy?: Strategy;

  constructor(
    private readonly connection: Connection,
    private readonly wallet: Keypair,
    private readonly buyLamports: bigint,
    private readonly buySlippageBps: number,
    private readonly sellSlippageBps: number,
    private readonly maxEntryMarketCapSol: number,
    private readonly blockhashes: BlockhashManager,
    private readonly sender: HeliusSender,
    private readonly confirmations: ConfirmationTracker,
    private readonly journal: RecoveryJournal,
    private readonly pnlJournal: PnlJournal,
    private readonly logger: Logger,
    private readonly strategyName: string,
    private readonly mintTrades: MintTradeLock = new MintTradeLock()
  ) {}

  bindStrategy(strategy: Strategy): void {
    this.#strategy = strategy;
  }

  recordEntryEvent(state: TokenState, event: string, extra: object = {}): void {
    this.#record(state, event, extra);
  }

  sendBuy(state: TokenState, signalMonoMs: number): Promise<void> {
    return this.mintTrades.run(this.#tradeKey(state.descriptor.mint), () => this.#sendBuy(state, signalMonoMs));
  }

  sendSell(state: TokenState, reason: string, signalMonoMs: number): Promise<void> {
    return this.mintTrades.run(this.#tradeKey(state.descriptor.mint), () => this.#sendSell(state, reason, signalMonoMs));
  }

  /** Same wallet still trades one mint at a time. A different wallet can trade that mint in parallel. */
  #tradeKey(mint: string): string {
    return `${this.wallet.publicKey.toBase58()}:${mint}`;
  }

  async #sendBuy(state: TokenState, signalMonoMs: number): Promise<void> {
    let lastError: unknown;
    const buySlippage = state.buySlippageBps ?? this.buySlippageBps;
    // The strategy may have built this before the mint lock. Rebuild so the quote follows the previous trade.
    state.preparedBuy = undefined;
    for (let attempt = 1; attempt <= 3; attempt++) {
      try {
        if (attempt > 1 && this.maxEntryMarketCapSol > 0 && state.prices.currentMarkPrice) {
          const currentPrice = state.prices.currentMarkPrice;
          const marketCapSol = entryMarketCapSol(currentPrice);
          if (!isEntryMarketCapAllowed(currentPrice, this.maxEntryMarketCapSol)) {
            const error = new Error(`buy retry aborted: market cap ${marketCapSol.toFixed(4)} SOL is at or above ${this.maxEntryMarketCapSol} SOL`);
            this.logger.warn({ attempt, mint: state.descriptor.mint, currentPrice, marketCapSol, maxMarketCapSol: this.maxEntryMarketCapSol }, "[05 ENTRY] Buy retry blocked by market-cap limit");
            this.#rejectBuy(state, "buy_market_cap_rejected", error);
            return;
          }
        }
        if (attempt > 1 || !state.preparedBuy) {
          state.preparedBuy = await state.adapter.buildBuy({ descriptor: state.descriptor, owner: this.wallet.publicKey, lamports: state.buyLamports ?? this.buyLamports, slippageBps: buySlippage });
        }
        const transaction = state.preparedBuy.compile((await this.blockhashes.get()).blockhash);
        transaction.sign([this.wallet]);
        const submitted = await this.#submit(transaction, signalMonoMs);
        state.buySignature = submitted.signature;
        this.#record(state, "buy_sent", { attempt, signature: submitted.signature, timing: submitted.timing });
        const fill = await this.#confirm(submitted.signature, state, submitted.fallback);
        if (!fill.success || fill.tokenAmount <= 0n) throw new Error("buy fill could not be determined");
        state.transition(TokenLifecycleState.BUY_SENT);
        state.actualTokenAmount = fill.tokenAmount;
        state.actualEntrySolAmount = fill.solAmount;
        state.prices.actualEntryFillPrice = fill.price;
        state.entryProcessedMs = Date.now();
        state.transition(TokenLifecycleState.BUY_PROCESSED);
        state.transition(TokenLifecycleState.POSITION_ACTIVE_UNCONFIRMED);
        this.#record(state, "buy_processed", { fill });
        state.transition(TokenLifecycleState.BUY_CONFIRMED);
        state.transition(TokenLifecycleState.POSITION_ACTIVE_CONFIRMED);
        this.#record(state, "buy_confirmed");
        this.#strategy?.onBuyFill?.(state, { price: fill.price, slot: 0 });
        return;
      } catch (error) {
        lastError = error;
        this.logger.warn({ attempt, err: error instanceof Error ? error.message : String(error), mint: state.descriptor.mint }, "[05 ENTRY] Buy attempt failed");
        if (isNonRetryableBuyError(error, state.descriptor.venue)) {
          this.logger.warn({ attempt, mint: state.descriptor.mint, venue: state.descriptor.venue }, "[05 ENTRY] Buy retry blocked after slippage failure");
          this.#rejectBuy(state, "buy_slippage_rejected", error);
          return;
        }
      }
    }
    this.#rejectBuy(state, "buy_failed", lastError);
  }

  async #sendSell(state: TokenState, reason: string, signalMonoMs: number): Promise<void> {
    if (!state.actualTokenAmount) { this.#sellFailed(state, reason, new Error("position token amount is unknown")); return; }
    let lastError: unknown;
    const sellSlippage = state.sellSlippageBps ?? this.sellSlippageBps;
    for (let attempt = 1; attempt <= 3; attempt++) {
      try {
        state.preparedSell = await state.adapter.buildSell({ descriptor: state.descriptor, owner: this.wallet.publicKey, tokenAmount: state.actualTokenAmount, slippageBps: sellSlippage });
        if (state.lifecycle !== TokenLifecycleState.SELL_PREPARED) state.transition(TokenLifecycleState.SELL_PREPARED);
        const transaction = state.preparedSell.compile((await this.blockhashes.get()).blockhash);
        transaction.sign([this.wallet]);
        const submitted = await this.#submit(transaction, signalMonoMs);
        state.sellSignature = submitted.signature;
        this.#record(state, "sell_sent", { attempt, reason, signature: submitted.signature, timing: submitted.timing });
        const fill = await this.#confirm(submitted.signature, state, submitted.fallback);
        if (!fill.success) throw new Error("sell fill could not be determined");
        state.transition(TokenLifecycleState.SELL_SENT);
        state.prices.actualExitFillPrice = fill.price;
        this.#record(state, "position_closed", { reason, fill });
        const closedAtMs = Date.now();
        if (state.actualEntrySolAmount && state.prices.actualEntryFillPrice && state.entryProcessedMs) {
          try {
            await this.pnlJournal.record({
              strategy: this.strategyName,
              closedAtMs,
              descriptor: state.descriptor,
              buyLamports: state.actualEntrySolAmount,
              sellLamports: fill.solAmount,
              tokenAmount: fill.tokenAmount,
              entryPrice: state.prices.actualEntryFillPrice,
              exitPrice: fill.price,
              prices: state.prices,
              exitReason: reason,
              buySignature: state.buySignature,
              sellSignature: state.sellSignature,
              entryProcessedMs: state.entryProcessedMs
            });
            this.logger.info({ mint: state.descriptor.mint, strategy: this.strategyName, pnlLamports: fill.solAmount - state.actualEntrySolAmount }, "[PNL] Closed trade appended");
          } catch (error) {
            this.logger.error({ err: error instanceof Error ? error.message : String(error), mint: state.descriptor.mint }, "[PNL] Failed to append closed trade");
          }
        } else this.logger.error({ mint: state.descriptor.mint }, "[PNL] Exact PNL unavailable: confirmed BUY fill amount is missing");

        const closed = this.#strategy?.onSellFill?.(state);
        if (closed?.thenBuy) {
          state.clearFilledPosition();
          state.resetBuySendClaim();
          state.resetSellSendClaim();
          state.transition(TokenLifecycleState.TRACKING_POOL);
          this.logger.info({ mint: state.descriptor.mint }, "[REENTRY] Fire-then-buy: preparing scalp entry");
          await this.#strategy?.onThenBuy?.(state);
        } else if (closed?.rearm) {
          state.clearFilledPosition();
          state.resetBuySendClaim();
          state.resetSellSendClaim();
          state.transition(TokenLifecycleState.TRACKING_POOL);
          this.logger.info({ mint: state.descriptor.mint }, "[REENTRY] Round finished; still watching for the next target buy");
        } else {
          state.transition(TokenLifecycleState.CLOSED);
        }
        return;
      } catch (error) {
        lastError = error;
        state.preparedSell = undefined;
        this.logger.warn({ attempt, err: error instanceof Error ? error.message : String(error), mint: state.descriptor.mint }, "[08 EXIT] Sell attempt failed");
      }
    }
    this.#sellFailed(state, reason, lastError);
  }

  async #confirm(signature: string, state: TokenState, fallback = false) {
    // RPC fallback after a dead sender often returns a signature the cluster never sees. Don't sit on it.
    const unseenTimeoutMs = fallback ? 10_000 : undefined;
    for (let poll = 1; poll <= 3; poll++) {
      try { return await this.confirmations.waitProcessed(signature, state.adapter, this.wallet.publicKey, state.descriptor.mint, unseenTimeoutMs); }
      catch (error) {
        if (!(error instanceof ConfirmationTimeoutError) || !error.seen || poll === 3) throw error;
        this.logger.warn({ poll, signature }, "[06 CONFIRM] Transaction pending; polling same signature");
      }
    }
    throw new ConfirmationTimeoutError(`confirmation timed out for ${signature}`, false);
  }

  async #submit(transaction: VersionedTransaction, signalMonoMs: number): Promise<{ signature: string; timing: SenderTiming; fallback: boolean }> {
    const serialized = transaction.serialize();
    const base64 = Buffer.from(serialized).toString("base64");
    let lastError: unknown;
    for (let attempt = 1; attempt <= 3; attempt++) {
      try { return { ...(await this.sender.send(base64, signalMonoMs)), fallback: false }; }
      catch (error) {
        lastError = error;
        this.logger.warn({ attempt, err: error instanceof Error ? error.message : String(error) }, "[06 CONFIRM] Helius submission attempt failed");
        if (error instanceof HeliusSenderUnavailableError) break;
      }
    }
    const sendStartMonoMs = performance.now();
    try {
      const signature = await this.connection.sendRawTransaction(serialized, { skipPreflight: false, maxRetries: 2 });
      return { signature, timing: { signalMonoMs, sendStartMonoMs, responseMonoMs: performance.now() }, fallback: true };
    } catch (fallbackError) {
      throw new AggregateError([lastError, fallbackError], "Sender and RPC fallback both failed");
    }
  }

  #record(state: TokenState, event: string, extra: object = {}): void {
    const value = {
      strategy: this.strategyName,
      event,
      descriptor: state.descriptor,
      lifecycle: state.lifecycle,
      actualTokenAmount: state.actualTokenAmount,
      actualEntrySolAmount: state.actualEntrySolAmount,
      buyLamports: state.buyLamports,
      buySignature: state.buySignature,
      sellSignature: state.sellSignature,
      prices: state.prices,
      entryProcessedMs: state.entryProcessedMs,
      ...extra
    };
    this.journal.record(value);
    const messages: Record<string, string> = {
      entry_market_cap_rejected: "[05 ENTRY] Buy rejected by market-cap limit",
      buy_market_cap_rejected: "[05 ENTRY] Buy retry rejected by market-cap limit",
      buy_sent: "[05 ENTRY] Buy transaction submitted",
      buy_processed: "[06 CONFIRM] Buy processed; fill calculated",
      buy_confirmed: "[07 POSITION] Buy confirmed; position active",
      sell_sent: "[08 EXIT] Sell transaction submitted",
      position_closed: "[08 EXIT] Sell confirmed; position closed"
    };
    this.logger.info(value, messages[event] ?? `[WORKFLOW] `);
  }

  #sellFailed(state: TokenState, reason: string, error: unknown): void {
    try { if (state.lifecycle === TokenLifecycleState.SELL_PREPARED) state.transition(TokenLifecycleState.POSITION_ACTIVE_CONFIRMED); } catch {}
    state.releaseSellSend();
    state.preparedSell = undefined;
    this.#strategy?.onSellFailed?.(state);
    const err = error instanceof Error ? error.message : String(error);
    this.journal.record({ strategy: this.strategyName, event: "sell_retry_exhausted", descriptor: state.descriptor, lifecycle: state.lifecycle, actualTokenAmount: state.actualTokenAmount, actualEntrySolAmount: state.actualEntrySolAmount, prices: state.prices, entryProcessedMs: state.entryProcessedMs, reason, error: err });
    this.logger.error({ err, reason, mint: state.descriptor.mint }, "[08 EXIT] Sell retries exhausted; position still monitored");
  }

  #rejectBuy(state: TokenState, event: string, error: unknown): void {
    const rearm = this.#strategy?.onBuyFailed?.(state)?.rearm === true;
    if (!rearm) {
      this.#fail(state, event, error);
      return;
    }
    try {
      if (state.lifecycle === TokenLifecycleState.BUY_PREPARED) state.transition(TokenLifecycleState.TRACKING_POOL);
    } catch { /* already tracking or terminal */ }
    state.resetBuySendClaim();
    state.preparedBuy = undefined;
    const err = error instanceof Error ? error.message : String(error);
    this.journal.record({ strategy: this.strategyName, event, descriptor: state.descriptor, lifecycle: state.lifecycle, error: err });
    this.logger.warn({ err, mint: state.descriptor.mint, pool: state.descriptor.pool }, "[05 ENTRY] Buy failed; still watching this mint");
  }

  #fail(state: TokenState, event: string, error: unknown): void {
    try { state.transition(TokenLifecycleState.FAILED); } catch {}
    const err = error instanceof Error ? error.message : String(error);
    this.journal.record({ strategy: this.strategyName, event, descriptor: state.descriptor, lifecycle: state.lifecycle, error: err });
    this.logger.error({ err, mint: state.descriptor.mint, pool: state.descriptor.pool }, `[ERROR] `);
  }
}
