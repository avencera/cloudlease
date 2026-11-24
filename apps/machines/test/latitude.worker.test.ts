import { runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeDigitalOcean } from "../../../tests/worker/test/fake-digitalocean";
import { FakeLatitude, LATITUDE_PROJECT, recordDescription, type Server } from "./fake-latitude";
import { FakeNtfy } from "./fake-ntfy";
import { FakePhoenixNap } from "./fake-phoenixnap";
import {
  call,
  CLIENT,
  createMachine,
  createRun,
  editHolding,
  gateStub,
  type LeaseView,
  machineBody,
  MINUTE,
  permits,
  readLease,
  runStub,
  scheduled,
  settle,
} from "./support";

let latitude: FakeLatitude;
let ntfy: FakeNtfy;

// the adapter names a server `clm-` and 16 hex digits, not the client's hostname, so the
// suite finds a machine's server through its user data record
function serverOf(runId: string, generation = 1): Server {
  const record = [...latitude.userData.values()].find((entry) =>
    entry.description.includes(`clm-owner-${runId} clm-gen-${generation} `),
  );
  expect(record).toBeDefined();
  const server = [...latitude.servers.values()].find((entry) => atob(record?.content ?? "") === entry.userData);
  expect(server).toBeDefined();
  return server as Server;
}

function recordOf(runId: string, generation = 1) {
  return [...latitude.userData.values()].find((entry) =>
    entry.description.includes(`clm-owner-${runId} clm-gen-${generation} `),
  );
}

function leaseToken(server: Server): string {
  const match = /cloudlease_write '\/etc\/cloudlease\/lease\.json' 0600 '([A-Za-z0-9+/=]+)'/.exec(server.userData);
  expect(match).not.toBeNull();
  return JSON.parse(atob(match?.[1] ?? "")).token;
}

// the hostname the adapter gives the server of one owner and generation
async function hostnameOf(owner: string, generation: number, prefix = "clm"): Promise<string> {
  const input = new TextEncoder().encode(`${prefix}\0${owner}\0${generation}`);
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", input));
  const hex = [...digest.slice(0, 8)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${prefix}-${hex}`;
}

// moves a releasing lease past the two minute tag search, and its retry with it
async function pastTagSearch(lease: LeaseView): Promise<void> {
  await editHolding(lease, (holding) => {
    const state = holding.state as unknown as { since: number; retry?: { at: number } };
    state.since -= 2 * MINUTE;
    if (state.retry !== undefined) {
      state.retry.at = Date.now();
    }
  });
}

async function latRun(overrides: Record<string, unknown> = {}) {
  return createRun({ class: "lat", name: "pool-lat", ...overrides });
}

describe("Latitude.sh machines in miniflare", () => {
  beforeEach(async () => {
    // the sweep lists DigitalOcean and phoenixNAP too
    new FakeDigitalOcean().install();
    new FakePhoenixNap().install();
    latitude = new FakeLatitude();
    latitude.install();
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

  it("creates the owner's user data record, then an hourly server that boots from it", async () => {
    const run = await latRun();
    const lease = await createMachine(run, "hb-lat-1");

    const server = serverOf(run.run_id);
    expect(server).toMatchObject({
      project: LATITUDE_PROJECT,
      site: "DAL",
      plan: "m4-metal-medium",
      operatingSystem: "ubuntu_24_04_x64_lts",
      billing: "hourly",
    });
    expect(server.hostname).toMatch(/^clm-[0-9a-f]{16}$/);
    expect(server.userData).toMatch(/^#!\/bin\/sh\n/);
    expect(server.userData).toContain("systemctl");
    expect(leaseToken(server)).toMatch(/^cll_[0-9a-f]{64}$/);
    const until = Math.floor(Date.parse(lease.hard_deadline_at) / 1000);
    expect(recordOf(run.run_id)).toMatchObject({
      project: LATITUDE_PROJECT,
      description: recordDescription(run.run_id, 1, until),
    });
    expect(latitude.log.filter((entry) => entry.startsWith("POST"))).toEqual(["POST user_data", "POST servers DAL"]);

    // the class's 15 minute boot grace is what the lease shows homebased
    expect(lease.state).toBe("running");
    expect(Date.parse(lease.heartbeat_deadline_at) - Date.parse(lease.created_at)).toBe(15 * MINUTE);
    expect(await permits()).toEqual([expect.objectContaining({ ownerId: run.run_id, generation: 1, units: 2 })]);
  });

  it("moves to the next site when the first has no stock, and fails the create after the last", async () => {
    latitude.outOfStock.add("DAL");
    // a 5 cent cap holds two $0.02 hours; the refused create must not take a third
    const run = await latRun({ spend_cap_cents: 5 });

    await createMachine(run, "hb-lat-stock-1");
    expect(serverOf(run.run_id, 1).site).toBe("CHI");
    expect(latitude.serverCreates()).toEqual(["POST servers DAL", "POST servers CHI"]);

    latitude.outOfStock.add("CHI");
    const response = await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-lat-stock-2"));
    expect(response.status).toBe(201);
    const failed = (await response.json()) as LeaseView;
    expect(failed.release).toMatchObject({ reason: "create_failed", by: "cloudlease" });
    expect(ntfy.titles()).toEqual(["cloudlease: machine create failed in run pool-lat"]);
    expect(ntfy.sent[0]?.body).toContain("no m4-metal-medium in stock at DAL, CHI");
    // the unbilled user data record does not count as a created machine, so its permit is
    // back at once, and its cleanup deletes the record
    expect(await permits()).toEqual([expect.objectContaining({ ownerId: run.run_id, generation: 1 })]);
    await pastTagSearch(failed);
    expect((await settle(failed)).state).toBe("released");
    expect(recordOf(run.run_id, 2)).toBeUndefined();

    // nothing billed was created, so once stock returns the run still has room for a second hour
    latitude.outOfStock.clear();
    await createMachine(run, "hb-lat-stock-3");
    expect(serverOf(run.run_id, 3).site).toBe("DAL");
  });

  it("renews, and deletes a released server, then its record, and confirms both gone", async () => {
    const run = await latRun();
    const lease = await createMachine(run, "hb-lat-release");
    const server = serverOf(run.run_id);
    const record = recordOf(run.run_id);
    const token = leaseToken(server);

    const renewed = await call("POST", `/v1/leases/${lease.lease_id}/renew`, token, {});
    expect(renewed.status).toBe(200);

    const response = await call("POST", `/v1/leases/${lease.lease_id}/release`, token, { reason: "done" });
    expect(response.status).toBe(202);
    const released = await settle(lease);
    expect(released.state).toBe("released");
    expect(released.release).toMatchObject({ reason: "done", by: "machine" });
    expect(latitude.deletesOf(server.id, record?.id ?? "none")).toEqual([
      `DELETE servers/${server.id}`,
      `DELETE user_data/${record?.id}`,
    ]);
    expect(latitude.servers.has(server.id)).toBe(false);
    expect(latitude.userData.has(record?.id ?? "")).toBe(false);
    expect(await permits()).toEqual([]);
  });

  it("deletes the server and record of a machine whose heartbeat lapsed", async () => {
    const run = await latRun();
    const lease = await createMachine(run, "hb-lat-lapse");
    const server = serverOf(run.run_id);
    const record = recordOf(run.run_id);

    await editHolding(lease, (holding) => {
      holding.state.lease.heartbeatDueAt = Date.now() - 1_000;
    });
    const released = await settle(lease);

    expect(released.release).toMatchObject({ reason: "heartbeat_lapsed", by: "cloudlease" });
    expect(released.state).toBe("released");
    expect(latitude.servers.has(server.id)).toBe(false);
    expect(latitude.userData.has(record?.id ?? "")).toBe(false);
  });

  it("sweeps a server that Latitude.sh listed only after its lease's cleanup gave up", async () => {
    latitude.loseCreates = true;
    const run = await latRun({ name: "pool-lost" });
    const response = await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-lat-lost"));
    expect(response.status).toBe(201);
    const lease = (await response.json()) as LeaseView;
    expect(lease.release).toMatchObject({ reason: "create_failed" });

    // the tag search finds nothing for two minutes, then counts as proof, and the cleanup
    // deletes the record, the only thing that named the server's owner
    await runDurableObjectAlarm(runStub(run.run_id));
    await pastTagSearch(lease);
    expect((await settle(lease)).state).toBe("released");
    expect(recordOf(run.run_id)).toBeUndefined();

    // Latitude.sh then lists the server, which no record names
    const lost = [...latitude.servers.values()].find((server) => server.hidden);
    expect(lost).toBeDefined();
    const id = lost?.id ?? "";
    if (lost !== undefined) {
      lost.hidden = false;
    }
    // a hand-named server in the namespace, but not in the adapter's shape, is left alone
    const hand = latitude.addServer("clm-build-box", new Date(Date.now() - 60 * MINUTE));
    // another project's twin is never seen
    const elsewhere = latitude.addServer(lost?.hostname ?? "", new Date(Date.now() - 60 * MINUTE), "proj_other");

    // inside the settle window the sweep only reports it
    await scheduled();
    expect(latitude.servers.has(id)).toBe(true);

    // once ten minutes old it is deleted as an orphan, and the alert says so
    if (lost !== undefined) {
      lost.createdAt = new Date(Date.now() - 11 * MINUTE).toISOString();
    }
    await scheduled();
    expect(latitude.deletesOf(id)).toEqual([`DELETE servers/${id}`]);
    expect(latitude.servers.has(id)).toBe(false);
    expect(latitude.servers.has(hand)).toBe(true);
    expect(latitude.servers.has(elsewhere)).toBe(true);
    expect(ntfy.titles()).toContain("cloudlease: sweep deleted 1 orphaned resources");
  });

  it("sweeps an unclaimed record and its server past their ceiling", async () => {
    const run = await latRun();
    await createMachine(run, "hb-lat-claimed");
    const claimed = serverOf(run.run_id);
    expect(claimed.hostname).toBe(await hostnameOf(run.run_id, 1));
    // a ghost generation: its record and server, past their ceiling, from a run object that
    // never existed
    const past = Math.floor(Date.now() / 1000) - 10;
    latitude.userData.set("ud_ghost", {
      id: "ud_ghost",
      project: LATITUDE_PROJECT,
      description: recordDescription("run_000000000000000000000000", 1, past),
      content: btoa("#!/bin/sh\n"),
    });
    const ghost = latitude.addServer(await hostnameOf("run_000000000000000000000000", 1), new Date());

    await scheduled();

    expect(latitude.deletesOf(ghost, "ud_ghost")).toEqual([`DELETE servers/${ghost}`, "DELETE user_data/ud_ghost"]);
    expect(latitude.servers.has(ghost)).toBe(false);
    expect(latitude.userData.has("ud_ghost")).toBe(false);
    expect(latitude.servers.has(claimed.id)).toBe(true);
    expect(recordOf(run.run_id)).toBeDefined();
  });

  it("reports latitude on health", async () => {
    const response = await call("GET", "/v1/health", null);
    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({ ok: true, phoenixnap: true, latitude: true });
  });
});
