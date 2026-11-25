// the Worker without phoenixNAP or Latitude.sh credentials and with broken AWS settings
// (vitest.machines-bare.config.ts)

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeDigitalOcean } from "../../../tests/worker/test/fake-digitalocean";
import { FakeNtfy } from "./fake-ntfy";
import { call, CLIENT, createMachine, createRun, expectError, runBody, scheduled } from "./support";

describe("a Worker without phoenixNAP or Latitude.sh credentials and with broken AWS settings", () => {
  let digitalocean: FakeDigitalOcean;
  let ntfy: FakeNtfy;

  beforeEach(() => {
    digitalocean = new FakeDigitalOcean();
    digitalocean.install();
    ntfy = new FakeNtfy();
    ntfy.install();
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

  it("refuses an AWS class whose settings are broken with a clear error", async () => {
    const response = await call("POST", "/v1/runs", CLIENT, runBody({ class: "spot" }));
    expect(response.status).toBe(400);
    const body = (await response.json()) as { error: { code: string; message: string } };
    expect(body.error.code).toBe("unknown_class");
    expect(body.error.message).toContain("settings on this Worker are broken");
    expect(body.error.message).toContain("AWS_SECRET_ACCESS_KEY");
  });

  it("reports the missing credentials on health, since a class needs them", async () => {
    const response = await call("GET", "/v1/health", null);
    expect(response.status).toBe(503);
    expect(await response.json()).toMatchObject({
      ok: false,
      digitalocean: true,
      phoenixnap: false,
      latitude: false,
      aws: false,
    });
    await expectError(await call("POST", "/v1/runs", CLIENT, runBody({ class: "metal" })), 400, "unknown_class");
  });

  it("sweeps broken AWS settings as a provider it cannot list, and alerts", async () => {
    // the Worker may have AWS machines it can no longer see, so the sweep must not count its
    // inventory as complete; unset phoenixNAP and Latitude.sh are simply left out
    await scheduled();
    await scheduled();
    expect(ntfy.titles()).toEqual([]);
    await scheduled();

    expect(ntfy.titles()).toEqual(["cloudlease: the sweep cannot list aws"]);
    expect(ntfy.sent[0]?.body).toContain("AWS_SECRET_ACCESS_KEY");
  });
});
