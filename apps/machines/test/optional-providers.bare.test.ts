// the Worker without phoenixNAP or Latitude.sh credentials (vitest.machines-bare.config.ts)

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeDigitalOcean } from "../../../tests/worker/test/fake-digitalocean";
import { FakeNtfy } from "./fake-ntfy";
import { call, CLIENT, createMachine, createRun, expectError, runBody } from "./support";

describe("a Worker without phoenixNAP or Latitude.sh credentials", () => {
  let digitalocean: FakeDigitalOcean;

  beforeEach(() => {
    digitalocean = new FakeDigitalOcean();
    digitalocean.install();
    new FakeNtfy().install();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("refuses a phoenixNAP class with a clear error and keeps serving DigitalOcean classes", async () => {
    const response = await call("POST", "/v1/runs", CLIENT, runBody({ class: "metal" }));
    expect(response.status).toBe(400);
    const body = (await response.json()) as { error: { code: string; message: string } };
    expect(body.error.code).toBe("unknown_class");
    expect(body.error.message).toContain("PNAP_CLIENT_ID");

    const run = await createRun();
    await createMachine(run, "hb-bare-1");
    expect(digitalocean.droplets.size).toBe(1);
  });

  it("refuses a Latitude.sh class with a clear error", async () => {
    const response = await call("POST", "/v1/runs", CLIENT, runBody({ class: "lat" }));
    expect(response.status).toBe(400);
    const body = (await response.json()) as { error: { code: string; message: string } };
    expect(body.error.code).toBe("unknown_class");
    expect(body.error.message).toContain("LATITUDE_API_KEY");
  });

  it("reports the missing credentials on health, since a class needs them", async () => {
    const response = await call("GET", "/v1/health", null);
    expect(response.status).toBe(503);
    expect(await response.json()).toMatchObject({
      ok: false,
      digitalocean: true,
      phoenixnap: false,
      latitude: false,
    });
    await expectError(await call("POST", "/v1/runs", CLIENT, runBody({ class: "metal" })), 400, "unknown_class");
  });
});
