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

// the Latitude.sh key, project and fake API; a Worker without them refuses Latitude.sh classes
export const LATITUDE_BINDINGS = {
  LATITUDE_API_KEY: "test-latitude-key",
  LATITUDE_PROJECT: "proj_test",
  LATITUDE_API_URL: "https://api.latitude.test/",
};

// the machines Worker's own AWS keys, its one region and the fake EC2 endpoint; a Worker
// without them refuses AWS classes
export const AWS_BINDINGS = {
  AWS_ACCESS_KEY_ID: "test-aws-key",
  AWS_SECRET_ACCESS_KEY: "test-aws-secret",
  AWS_REGIONS: "us-east-1",
  AWS_EC2_URL: "https://ec2.aws.test/",
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
                  ssh_key_ids: ["5fa54d1e91867c03a0a7b4a4"],
                  price_per_hour_usd: "0.02",
                  max_hours: 12,
                },
                // a Latitude.sh server, billed by the started hour until its rounding is known
                lat: {
                  provider: "latitude",
                  plan: "m4-metal-medium",
                  sites: ["DAL", "CHI"],
                  operating_system: "ubuntu_24_04_x64_lts",
                  price_per_hour_usd: "0.02",
                  max_hours: 12,
                },
                // an AWS Spot instance, billed per second; one fleet offers two types in two zones
                spot: {
                  provider: "aws",
                  region: "us-east-1",
                  instance_types: ["c8a.4xlarge", "c7a.4xlarge"],
                  zones: [
                    { zone: "us-east-1a", subnet_id: "subnet-0123456789abcdef0" },
                    { zone: "us-east-1b", subnet_id: "subnet-0123456789abcdef1" },
                  ],
                  image_id: "ami-0123456789abcdef0",
                  root_volume_gib: 150,
                  launch_template_id: "lt-0123456789abcdef0",
                  launch_template_version: 3,
                  spot_max_price_usd: "0.025",
                  price_per_hour_usd: "0.03",
                  max_hours: 12,
                },
              },
              apps: {
                homebased: { classes: ["small", "huge", "metal", "lat", "spot"] },
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
