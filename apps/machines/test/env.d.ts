// the bindings of apps/machines/wrangler.jsonc and vitest.machines.config.ts, which the
// suite reads through `env` from "cloudflare:workers"

declare namespace Cloudflare {
  interface Env {
    RUNS: DurableObjectNamespace;
    GATE: DurableObjectNamespace;
    MONITOR: DurableObjectNamespace;
    CLOUDLEASE_TAG_PREFIX: string;
  }
}
