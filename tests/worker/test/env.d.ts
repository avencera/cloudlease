// the bindings of tests/worker/wrangler.jsonc and vitest.worker.config.ts, which the
// suite reads through `env` from "cloudflare:workers"

declare namespace Cloudflare {
  interface Env {
    OWNERS: DurableObjectNamespace;
    CAPACITY: DurableObjectNamespace;
    DIGITALOCEAN_TOKEN: string;
    DIGITALOCEAN_API_URL: string;
    CLOUDLEASE_TAG_PREFIX: string;
    MAX_ACTIVE_MACHINES: string;
  }
}
