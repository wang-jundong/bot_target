import { describe, expect, it } from "vitest";
import { HeliusSender, HeliusSenderUnavailableError, signatureFromSenderBody } from "../src/helius/sender.js";

describe("disabled live submission safety", () => {
  it("blocks submission before making an HTTP request", async () => {
    const sender = new HeliusSender("https://sender.helius-rpc.com/fast", true, false);
    await expect(sender.send("not-a-real-transaction", performance.now())).rejects.toThrow("LIVE_SUBMISSION_BLOCKED");
    await sender.close();
  });

  it("treats an HTML gateway page as sender unavailable", () => {
    expect(() => signatureFromSenderBody(502, "<html><body>bad gateway</body></html>")).toThrow(HeliusSenderUnavailableError);
    expect(() => signatureFromSenderBody(200, "not-json")).toThrow(HeliusSenderUnavailableError);
  });

  it("reads a JSON-RPC signature and still reports a rejected transaction", () => {
    expect(signatureFromSenderBody(200, JSON.stringify({ result: "sig" }))).toBe("sig");
    expect(() => signatureFromSenderBody(200, JSON.stringify({ error: { message: "blockhash not found" } }))).toThrow("blockhash not found");
  });
});
