import "dotenv/config";
import { createRequire } from "node:module";
import { CommitmentLevel } from "@triton-one/yellowstone-grpc";

const endpoint = process.env.VIBE_GRPC_ENDPOINT?.trim();
const token = process.env.VIBE_GRPC_TOKEN?.trim();

if (!endpoint) throw new Error("VIBE_GRPC_ENDPOINT is missing from .env");
if (!/^https?:\/\//i.test(endpoint)) throw new Error("VIBE_GRPC_ENDPOINT must start with https:// or http://");

// Vibe cloud plans use X-Token. Discord/IP-allowlisted plans intentionally omit it.
const require = createRequire(import.meta.url);
interface SmokeClient {
  connect(): Promise<void>;
  ping(count: number): Promise<{ count: number }>;
  getVersion(): Promise<{ version: string }>;
  getSlot(commitment?: CommitmentLevel): Promise<{ slot: string }>;
}
const YellowstoneClient = (require("@triton-one/yellowstone-grpc") as {
  default: new (endpoint: string, token: string | undefined, options: { grpcMaxDecodingMessageSize?: number }) => SmokeClient;
}).default;
const client = new YellowstoneClient(endpoint, token || undefined, {
  grpcMaxDecodingMessageSize: 16 * 1024 * 1024
});

const started = performance.now();
try {
  await client.connect();
  const [pong, version, slot] = await Promise.all([
    client.ping(1),
    client.getVersion(),
    client.getSlot(CommitmentLevel.PROCESSED)
  ]);
  console.log({
    ok: true,
    endpoint,
    authentication: token ? "x-token configured" : "IP allowlist/no token",
    roundTripMs: Number((performance.now() - started).toFixed(2)),
    pong: pong.count,
    version: version.version,
    slot: slot.slot
  });
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  console.error({ ok: false, endpoint, authentication: token ? "x-token configured" : "no token", error: message });
  process.exitCode = 1;
}
