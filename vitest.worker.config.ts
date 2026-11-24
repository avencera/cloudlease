import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: {
        configPath: "tests/worker/wrangler.jsonc",
      },
      miniflare: {
        bindings: {
          DIGITALOCEAN_TOKEN: "test-provider-token",
          DIGITALOCEAN_API_URL: "https://api.digitalocean.test/v2",
          CLOUDLEASE_TAG_PREFIX: "cl",
          MAX_ACTIVE_MACHINES: "2",
        },
      },
    }),
  ],
  test: {
    include: ["tests/worker/test/**/*.worker.test.ts"],
    testTimeout: 15_000,
  },
});
