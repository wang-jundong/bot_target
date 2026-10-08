import "dotenv/config";
import Yellowstone, { CommitmentLevel } from "@triton-one/yellowstone-grpc";

type YellowstoneClient = typeof import("@triton-one/yellowstone-grpc").default;
// The ESM build exports the class as default. TypeScript types the package as CommonJS, so the import needs this cast.
const Client = Yellowstone as unknown as YellowstoneClient;

const endpoint = process.env.VIBE_GRPC_ENDPOINT?.trim();
const token = process.env.VIBE_GRPC_TOKEN?.trim();

if (!endpoint) throw new Error("VIBE_GRPC_ENDPOINT is missing from .env");
if (!/^https?:\/\//i.test(endpoint)) throw new Error("VIBE_GRPC_ENDPOINT must start with https:// or http://");

// Vibe cloud plans use X-Token. Discord/IP-allowlisted plans intentionally omit it.
const client = new Client(endpoint, token || undefined, {
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
