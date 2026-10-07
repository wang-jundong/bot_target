import { Agent, request } from "undici";
export interface SenderTiming { signalMonoMs: number; sendStartMonoMs: number; responseMonoMs: number; }

/** Gateway or proxy returned a page instead of a JSON-RPC body. Retrying that socket will not help. */
export class HeliusSenderUnavailableError extends Error {
  constructor(readonly statusCode: number) {
    super(`Helius Sender unavailable: HTTP ${statusCode}`);
    this.name = "HeliusSenderUnavailableError";
  }
}

export function signatureFromSenderBody(statusCode: number, body: string): string {
  const trimmed = body.trimStart();
  if (trimmed.startsWith("<")) throw new HeliusSenderUnavailableError(statusCode);
  let json: { result?: string; error?: { message?: string } };
  try { json = JSON.parse(body) as { result?: string; error?: { message?: string } }; }
  catch { throw new HeliusSenderUnavailableError(statusCode); }
  if (json.result) return json.result;
  if (statusCode >= 500) throw new HeliusSenderUnavailableError(statusCode);
  throw new Error(`Helius Sender rejected transaction: ${json.error?.message ?? statusCode}`);
}

export class HeliusSender {
  #agent = HeliusSender.#newAgent();
  constructor(private readonly url: string, private readonly swqosOnly: boolean, private readonly liveEnabled = false) {}
  async send(base64Transaction: string, signalMonoMs: number): Promise<{ signature: string; timing: SenderTiming }> {
    if (!this.liveEnabled) throw new Error("LIVE_SUBMISSION_BLOCKED: EXECUTION_MODE is not live");
    const sendStartMonoMs = performance.now();
    const url = new URL(this.url); url.searchParams.set("swqos_only", String(this.swqosOnly));
    const body = `{"jsonrpc":"2.0","id":1,"method":"sendTransaction","params":["${base64Transaction}",{"encoding":"base64","skipPreflight":true,"maxRetries":0}]}`;
    const response = await request(url, { method: "POST", body, dispatcher: this.#agent, headers: { "content-type": "application/json" } });
    const text = await response.body.text();
    let signature: string;
    try { signature = signatureFromSenderBody(response.statusCode, text); }
    catch (error) {
      if (error instanceof HeliusSenderUnavailableError) this.#replaceAgent();
      throw error;
    }
    return { signature, timing: { signalMonoMs, sendStartMonoMs, responseMonoMs: performance.now() } };
  }
  async close(): Promise<void> { await this.#agent.close(); }

  #replaceAgent(): void {
    const previous = this.#agent;
    this.#agent = HeliusSender.#newAgent();
    void previous.close().catch(() => undefined);
  }

  static #newAgent(): Agent {
    return new Agent({ connections: 8, pipelining: 1, keepAliveTimeout: 60_000 });
  }
}
