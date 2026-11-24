import { createHash } from "node:crypto";
import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

const sha256 = (value: string) => createHash("sha256").update(value).digest("hex");

// the phoenixNAP credentials and fake API; a Worker without them refuses phoenixNAP classes
export const PHOENIXNAP_BINDINGS = {
  PNAP_CLIENT_ID: "test-pnap-client",
  PNAP_CLIENT_SECRET: "test-pnap-secret",
  PNAP_API_URL: "https://api.phoenixnap.test/",
  PNAP_TOKEN_URL: "https://auth.phoenixnap.test/token",
};

// the machines Worker in miniflare against the fake providers, with the suite's classes and
// client tokens "client-homebased" and "client-other"
export function machinesConfig(include: string[], extraBindings: Record<string, string>) {
  return defineConfig({
    plugins: [
      cloudflareTest({
        wrangler: {
          configPath: "apps/machines/wrangler.jsonc",
        },
        miniflare: {
          bindings: {
            DIGITALOCEAN_TOKEN: "test-provider-token",
            DIGITALOCEAN_API_URL: "https://api.digitalocean.test/v2",
            CLOUDLEASE_TAG_PREFIX: "clm",
            HOURLY_SPEND_CAP_CENTS: "100",
            NTFY_URL: "https://ntfy.test/cloudlease-alerts",
            NTFY_TOKEN: "test-ntfy-token",
            CLIENT_TOKENS: JSON.stringify({
              homebased: sha256("client-homebased"),
              other: sha256("client-other"),
            }),
            ...extraBindings,
            MACHINES_CONFIG: {
              api_base: "https://machines.test",
              max_run_hours: 24,
              max_machines_per_run: 8,
              run_spend_cap_usd: "0.05",
              classes: {
                small: {
                  region: "nyc3",
                  size: "s-1vcpu-1gb",
                  image: "ubuntu-24-04-x64",
                  price_per_hour_usd: "0.01",
                  max_hours: 12,
                },
                disk: {
                  region: "nyc3",
                  size: "s-1vcpu-1gb",
                  image: "ubuntu-24-04-x64",
                  price_per_hour_usd: "0.01",
                  max_hours: 12,
                  volume: { size_gib: 10, mount: "/mnt/data" },
                },
                // more per hour than the whole hourly cap, so the gate always refuses it
                huge: {
                  region: "nyc3",
                  size: "c-48",
                  image: "ubuntu-24-04-x64",
                  price_per_hour_usd: "1.50",
                  max_hours: 12,
                },
                // billed by the started hour, so a minimum is a whole hour's $0.02
                metal: {
                  provider: "phoenixnap",
                  server_type: "s2.c2.large",
                  locations: ["SEA", "NLD"],
                  os: "ubuntu/jammy",
                  price_per_hour_usd: "0.02",
                  max_hours: 12,
                },
              },
              apps: {
                homebased: { classes: ["small", "disk", "huge", "metal"] },
                other: { classes: ["small"] },
              },
            },
          },
        },
      }),
    ],
    test: {
      include,
      testTimeout: 15_000,
    },
  });
}
