import { runInDurableObject } from "cloudflare:test";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeDigitalOcean } from "../../../tests/worker/test/fake-digitalocean";
import { FakeNtfy } from "./fake-ntfy";
import { FakePhoenixNap, pnapTags, type Server } from "./fake-phoenixnap";
import {
  backdateMachine,
  call,
  CLIENT,
  createMachine,
  createRun,
  editHolding,
  expectError,
  gateStub,
  type LeaseView,
  machineBody,
  MINUTE,
  permits,
  readLease,
  readRun,
  scheduled,
  settle,
} from "./support";

let pnap: FakePhoenixNap;
let ntfy: FakeNtfy;

function serverOf(hostname: string): Server {
  const server = pnap.serverNamed(hostname);
  expect(server).toBeDefined();
  return server as Server;
}

// the lease file reaches the server only inside its cloud-init user data
function leaseToken(hostname: string): string {
  const match = /cloudlease_write '\/etc\/cloudlease\/lease\.json' 0600 '([A-Za-z0-9+/=]+)'/.exec(
    serverOf(hostname).userData,
  );
  expect(match).not.toBeNull();
  return JSON.parse(atob(match?.[1] ?? "")).token;
}

async function metalRun(overrides: Record<string, unknown> = {}) {
  return createRun({ class: "metal", name: "pool-metal", ...overrides });
}

describe("phoenixNAP machines in miniflare", () => {
  beforeEach(async () => {
    const digitalocean = new FakeDigitalOcean();
    digitalocean.dropletReadsAfterDelete = 0;
    digitalocean.install();
    pnap = new FakePhoenixNap();
    pnap.install();
    ntfy = new FakeNtfy();
    ntfy.install();

    // permits of earlier tests' machines would count against the hourly cap
    await permits();
    await runInDurableObject(gateStub(), (_instance, state) => {
      state.storage.sql.exec("DELETE FROM cloudlease_permits");
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("creates a server in the class's first location with its tags and boot data", async () => {
    const run = await metalRun();
    const lease = await createMachine(run, "hb-metal-1");

    const server = serverOf("hb-metal-1");
    expect(server).toMatchObject({ location: "SEA", type: "s2.c2.large", os: "ubuntu/jammy" });
    expect(server.tags).toEqual(
      expect.arrayContaining(pnapTags(run.run_id, 1, Math.floor(Date.parse(lease.hard_deadline_at) / 1000))),
    );
    expect(server.userData).toMatch(/^#!\/bin\/sh\n/);
    expect(server.userData).toContain("systemctl");
    expect(leaseToken("hb-metal-1")).toMatch(/^cll_[0-9a-f]{64}$/);
    expect(pnap.tagNames).toEqual(new Set(["clm-managed", "clm-owner", "clm-gen", "clm-kind", "clm-until"]));

    // the class's 20 minute boot grace is what the lease shows homebased
    expect(lease.state).toBe("running");
    expect(Date.parse(lease.heartbeat_deadline_at) - Date.parse(lease.created_at)).toBe(20 * MINUTE);
    expect(await permits()).toEqual([expect.objectContaining({ ownerId: run.run_id, generation: 1, units: 2 })]);
  });

  it("moves to the next location when the first has no stock, and fails the create after the last", async () => {
    pnap.outOfStock.add("SEA");
    // a 5 cent cap holds two $0.02 hours; the refused create must not take a third
    const run = await metalRun({ spend_cap_cents: 5 });

    await createMachine(run, "hb-metal-stock-1");
    expect(serverOf("hb-metal-stock-1").location).toBe("NLD");
    expect(pnap.creates()).toEqual(["POST bmc/v1/servers SEA", "POST bmc/v1/servers NLD"]);

    pnap.outOfStock.add("NLD");
    const response = await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-metal-stock-2"));
    expect(response.status).toBe(201);
    const failed = (await response.json()) as LeaseView;
    expect(failed.release).toMatchObject({ reason: "create_failed", by: "cloudlease" });
    expect(pnap.serverNamed("hb-metal-stock-2")).toBeUndefined();
    expect(ntfy.titles()).toEqual(["cloudlease: machine create failed in run pool-metal"]);
    expect(ntfy.sent[0]?.body).toContain("no s2.c2.large in stock at SEA, NLD");

    // nothing was created, so once stock returns the run still has room for a second hour
    pnap.outOfStock.clear();
    await createMachine(run, "hb-metal-stock-3");
    expect(serverOf("hb-metal-stock-3").location).toBe("SEA");
  });

  it("renews, and deletes a released server and confirms it gone", async () => {
    pnap.listsAfterDeprovision = 2;
    const run = await metalRun();
    const lease = await createMachine(run, "hb-metal-release");
    const token = leaseToken("hb-metal-release");
    const id = serverOf("hb-metal-release").id;

    const renewed = await call("POST", `/v1/leases/${lease.lease_id}/renew`, token, {});
    expect(renewed.status).toBe(200);

    const response = await call("POST", `/v1/leases/${lease.lease_id}/release`, token, { reason: "done" });
    expect(response.status).toBe(202);
    // the server is still listed as deleting after its deprovision, so the lease waits
    await vi.waitUntil(() => pnap.deprovisionsOf(id).length > 0, { timeout: 5_000 });
    expect((await readLease(lease.lease_id)).state).toBe("releasing");

    const released = await settle(lease);
    expect(released.state).toBe("released");
    expect(released.release).toMatchObject({ reason: "done", by: "machine" });
    expect(pnap.servers.has(id)).toBe(false);
    expect(await permits()).toEqual([]);
  });

  it("deletes a server whose heartbeat lapsed", async () => {
    const run = await metalRun();
    const lease = await createMachine(run, "hb-metal-lapse");
    const id = serverOf("hb-metal-lapse").id;

    await editHolding(lease, (holding) => {
      holding.state.lease.heartbeatDueAt = Date.now() - 1_000;
    });
    const released = await settle(lease);

    expect(released.state).toBe("released");
    expect(released.release).toMatchObject({ reason: "heartbeat_lapsed", by: "cloudlease" });
    expect(pnap.deprovisionsOf(id)).toHaveLength(1);
    expect(pnap.servers.has(id)).toBe(false);
  });

  it("deletes a server at its hard deadline", async () => {
    const run = await metalRun();
    const lease = await createMachine(run, "hb-metal-deadline");
    const id = serverOf("hb-metal-deadline").id;

    await editHolding(lease, (holding) => {
      const now = Date.now();
      holding.state.lease.startedAt = now - 3_000;
      holding.state.lease.lastHeartbeatAt = now - 3_000;
      holding.state.lease.heartbeatDueAt = now - 2_000;
      holding.state.lease.hardDeadline = now - 1_000;
    });
    const released = await settle(lease);

    expect(released.release).toMatchObject({ reason: "hard_deadline", by: "cloudlease" });
    expect(released.state).toBe("released");
    expect(pnap.servers.has(id)).toBe(false);
  });

  it("sweeps an unclaimed server past its ceiling and keeps a claimed one", async () => {
    const run = await metalRun();
    await createMachine(run, "hb-metal-sweep");
    const claimed = serverOf("hb-metal-sweep").id;
    const past = Math.floor(Date.now() / 1000) - 10;
    const ghost = pnap.addServer(pnapTags("run_000000000000000000000000", 1, past));
    // a server of a run object that never existed, still inside its ceiling
    const orphan = pnap.addServer(pnapTags("run_00000000000000000000000f", 2));
    // a near miss is never managed, so it is left alone
    const foreign = pnap.addServer([{ name: "clm-managed" }, { name: "clm-owner", value: run.run_id }]);

    await scheduled();

    expect(pnap.deprovisionsOf(ghost)).toHaveLength(1);
    expect(pnap.deprovisionsOf(orphan)).toHaveLength(1);
    expect(pnap.servers.has(ghost)).toBe(false);
    expect(pnap.servers.has(orphan)).toBe(false);
    expect(pnap.servers.has(claimed)).toBe(true);
    expect(pnap.servers.has(foreign)).toBe(true);
    expect(ntfy.titles()).toEqual(["cloudlease: sweep deleted 2 orphaned resources"]);
  });

  it("bills every started hour: a 5 minute server costs an hour, and a second hour starts at 60 minutes", async () => {
    // a 3 cent cap holds one $0.02 hour, not two
    const run = await metalRun({ spend_cap_cents: 3 });
    const lease = await createMachine(run, "hb-metal-spend-1");

    // after 5 minutes the server has cost its whole first hour, $0.02, so another machine's
    // hour does not fit; billed by the second it would have cost a cent and fitted
    await backdateMachine(lease, 5 * MINUTE);
    await expectError(
      await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-metal-spend-2")),
      429,
      "spend_cap_reached",
    );

    // the second hour starts at 60 minutes and takes the run to $0.04, past its cap
    await backdateMachine(lease, 55 * MINUTE + 1_000);
    const released = await settle(lease);
    expect(released.release).toMatchObject({ reason: "spend_cap", by: "cloudlease" });
    expect(released.state).toBe("released");
    expect((await readRun(run.run_id)).state).toBe("released");
    expect(ntfy.titles()).toEqual(["cloudlease: run pool-metal reached its spend cap"]);
  });
});
