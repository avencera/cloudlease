import {
  createExecutionContext,
  createScheduledController,
  runDurableObjectAlarm,
  runInDurableObject,
  SELF,
  waitOnExecutionContext,
} from "cloudflare:test";
import { env } from "cloudflare:workers";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeDigitalOcean, tags } from "../../../tests/worker/test/fake-digitalocean";
// scheduled controllers cannot cross the loopback binding, so the suite calls the shim directly
// @ts-expect-error worker-build emits no declaration for its JavaScript shim
import RustWorker from "../build/index.js";
import { FakeNtfy } from "./fake-ntfy";

const BASE = "https://machines.test";
const CLIENT = "client-homebased";
const OTHER_CLIENT = "client-other";
const MINUTE = 60_000;
const HOUR = 60 * MINUTE;

interface ReleaseView {
  reason: string;
  by: string;
  at: string;
}

interface LeaseView {
  lease_id: string;
  run_id: string;
  request_id: string;
  hostname: string;
  state: "provisioning" | "running" | "releasing" | "released";
  created_at: string;
  hard_deadline_at: string;
  heartbeat_deadline_at: string;
  release: ReleaseView | null;
  gone_at: string | null;
}

interface RunView {
  run_id: string;
  name: string;
  class: string;
  max_machines: number;
  deadline_at: string;
  state: "open" | "releasing" | "released";
  created_at: string;
  machines: LeaseView[];
}

interface ErrorBody {
  error: { code: string; message: string; retryable: boolean };
}

interface Permit {
  permitId: string;
  ownerId: string;
  generation: number;
  units: number;
}

let fake: FakeDigitalOcean;
let ntfy: FakeNtfy;

async function call(
  method: "GET" | "POST",
  path: string,
  token: string | null,
  body?: unknown,
): Promise<Response> {
  const headers: Record<string, string> = { "content-type": "application/json" };
  if (token !== null) {
    headers.authorization = `Bearer ${token}`;
  }
  return SELF.fetch(`${BASE}${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

async function expectError(response: Response, status: number, code: string): Promise<void> {
  expect(response.status).toBe(status);
  const body = (await response.json()) as ErrorBody;
  expect(body.error.code).toBe(code);
}

function runBody(overrides: Record<string, unknown> = {}) {
  return {
    request_id: crypto.randomUUID(),
    name: "pool-test",
    class: "small",
    max_machines: 4,
    deadline_at: new Date(Date.now() + 2 * HOUR).toISOString(),
    ...overrides,
  };
}

async function createRun(overrides: Record<string, unknown> = {}): Promise<RunView> {
  const response = await call("POST", "/v1/runs", CLIENT, runBody(overrides));
  expect(response.status).toBe(201);
  return response.json();
}

function machineBody(hostname: string) {
  return {
    request_id: crypto.randomUUID(),
    hostname,
    boot: {
      files: [{ path: "/etc/homebased/agent.toml", mode: "0600", content: "token = 'x'\n" }],
      run: [["systemctl", "start", "homebased-agent"]],
    },
  };
}

async function createMachine(run: RunView, hostname: string): Promise<LeaseView> {
  const response = await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody(hostname));
  expect(response.status).toBe(201);
  return response.json();
}

// the lease token reaches the machine only inside its user data
function leaseToken(hostname: string): string {
  const droplet = [...fake.droplets.values()].find((candidate) => candidate.name === hostname);
  expect(droplet).toBeDefined();
  const match = /cloudlease_write '\/etc\/cloudlease\/lease\.json' 0600 '([A-Za-z0-9+/=]+)'/.exec(
    droplet?.userData ?? "",
  );
  expect(match).not.toBeNull();
  const lease = JSON.parse(atob(match?.[1] ?? "")) as { token: string };
  return lease.token;
}

function dropletOf(hostname: string) {
  return [...fake.droplets.values()].find((droplet) => droplet.name === hostname);
}

async function readLease(leaseId: string, token = CLIENT): Promise<LeaseView> {
  const response = await call("GET", `/v1/leases/${leaseId}`, token);
  expect(response.status).toBe(200);
  return response.json();
}

async function readRun(runId: string): Promise<RunView> {
  const response = await call("GET", `/v1/runs/${runId}`, CLIENT);
  expect(response.status).toBe(200);
  return response.json();
}

function runStub(runId: string): DurableObjectStub {
  return env.RUNS.get(env.RUNS.idFromName(runId));
}

function gateStub(): DurableObjectStub {
  return env.GATE.get(env.GATE.idFromName("global"));
}

async function permits(): Promise<Permit[]> {
  const response = await gateStub().fetch("https://gate/cloudlease/capacity/snapshot");
  expect(response.status).toBe(200);
  return ((await response.json()) as { permits: Permit[] }).permits;
}

function generationOf(leaseId: string): number {
  return Number(leaseId.split("-").at(-1));
}

// moves a stored time into the past, as if the run object had slept through it
async function editHolding(
  lease: LeaseView,
  edit: (holding: { state: { lease: Record<string, number>; since: number } }) => void,
): Promise<void> {
  await runInDurableObject(runStub(lease.run_id), (_instance, state) => {
    const generation = generationOf(lease.lease_id);
    const row = state.storage.sql
      .exec("SELECT holding FROM cloudlease_holdings WHERE generation = ?", generation)
      .one();
    const holding = JSON.parse(row.holding as string);
    edit(holding);
    state.storage.sql.exec(
      "UPDATE cloudlease_holdings SET holding = ? WHERE generation = ?",
      JSON.stringify(holding),
      generation,
    );
  });
}

async function backdateMachine(lease: LeaseView, by: number): Promise<void> {
  await runInDurableObject(runStub(lease.run_id), (_instance, state) => {
    const generation = generationOf(lease.lease_id);
    const row = state.storage.sql
      .exec("SELECT record FROM machines_requests WHERE generation = ?", generation)
      .one();
    const record = JSON.parse(row.record as string);
    record.admission.created_at -= by;
    state.storage.sql.exec(
      "UPDATE machines_requests SET record = ? WHERE generation = ?",
      JSON.stringify(record),
      generation,
    );
  });
}

// runs the run object's alarm until the lease is released, waiting out cleanup retries,
// which are about a second apart
async function settle(lease: LeaseView): Promise<LeaseView> {
  const stub = runStub(lease.run_id);
  for (let pass = 0; pass < 10; pass += 1) {
    const current = await readLease(lease.lease_id);
    if (current.state === "released") {
      return current;
    }

    const at = await runInDurableObject(stub, (_instance, state) => state.storage.getAlarm());
    if (at === null) {
      break;
    }
    // a near alarm is a retry; a far one is a deadline the test moved, which is already due
    const wait = at - Date.now();
    if (wait > 0 && wait < 5_000) {
      await new Promise((resolve) => setTimeout(resolve, wait + 50));
    }
    // miniflare may have run a due alarm itself meanwhile
    await runDurableObjectAlarm(stub);
  }
  return readLease(lease.lease_id);
}

async function scheduled(): Promise<void> {
  const controller = createScheduledController({ cron: "*/5 * * * *", scheduledTime: new Date() });
  const context = createExecutionContext();
  const worker = new RustWorker(context, env);

  await worker.scheduled(controller);
  await waitOnExecutionContext(context);
}

describe("cloudlease machines in miniflare", () => {
  beforeEach(async () => {
    fake = new FakeDigitalOcean();
    fake.dropletReadsAfterDelete = 0;
    fake.install();
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

  it("says which settings work without a credential", async () => {
    const response = await call("GET", "/v1/health", null);

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      ok: true,
      config: true,
      client_tokens: true,
      digitalocean: true,
      ntfy: true,
    });
  });

  it("refuses requests without a client token", async () => {
    await expectError(await call("POST", "/v1/runs", null, runBody()), 401, "unauthorized");
    await expectError(await call("POST", "/v1/runs", "cll_not-a-client", runBody()), 401, "unauthorized");
  });

  it("creates a run once per request ID", async () => {
    const body = runBody({ name: "pool-idempotent" });

    const first = await call("POST", "/v1/runs", CLIENT, body);
    expect(first.status).toBe(201);
    const run = (await first.json()) as RunView;
    expect(run).toMatchObject({ name: "pool-idempotent", class: "small", state: "open", machines: [] });

    const again = await call("POST", "/v1/runs", CLIENT, body);
    expect(again.status).toBe(200);
    expect(((await again.json()) as RunView).run_id).toBe(run.run_id);

    const conflicting = await call("POST", "/v1/runs", CLIENT, { ...body, max_machines: 2 });
    await expectError(conflicting, 409, "request_conflict");

    // another app's run is invisible, and the same request ID names a different run
    await expectError(await call("GET", `/v1/runs/${run.run_id}`, OTHER_CLIENT), 404, "run_not_found");
    const other = await call("POST", "/v1/runs", OTHER_CLIENT, body);
    expect(other.status).toBe(201);
    expect(((await other.json()) as RunView).run_id).not.toBe(run.run_id);
  });

  it("refuses a run with a class, size or deadline outside the config", async () => {
    await expectError(await call("POST", "/v1/runs", CLIENT, runBody({ class: "gpu" })), 400, "unknown_class");
    await expectError(await call("POST", "/v1/runs", OTHER_CLIENT, runBody({ class: "huge" })), 400, "unknown_class");
    await expectError(await call("POST", "/v1/runs", CLIENT, runBody({ max_machines: 9 })), 400, "invalid_request");
    const tooFar = new Date(Date.now() + 25 * HOUR).toISOString();
    await expectError(await call("POST", "/v1/runs", CLIENT, runBody({ deadline_at: tooFar })), 400, "deadline_too_far");
  });

  it("creates a machine once per request ID and renders its boot data", async () => {
    const run = await createRun();
    const body = machineBody("hb-idempotent-1");

    const first = await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, body);
    expect(first.status).toBe(201);
    const lease = (await first.json()) as LeaseView;
    expect(lease).toMatchObject({
      lease_id: `${run.run_id}-1`,
      hostname: "hb-idempotent-1",
      state: "running",
      release: null,
    });

    const again = await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, body);
    expect(again.status).toBe(200);
    expect(((await again.json()) as LeaseView).lease_id).toBe(lease.lease_id);
    expect(fake.log.filter((entry) => entry === "POST droplets")).toHaveLength(1);

    const conflicting = { ...body, hostname: "hb-idempotent-2" };
    await expectError(await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, conflicting), 409, "request_conflict");

    const droplet = dropletOf("hb-idempotent-1");
    expect(droplet?.tags).toEqual(
      expect.arrayContaining(tags(run.run_id, 1, "droplet", undefined, "clm")),
    );
    expect(droplet?.userData).toContain("systemctl");
    expect(leaseToken("hb-idempotent-1")).toMatch(/^cll_[0-9a-f]{64}$/);
    expect(await permits()).toEqual([expect.objectContaining({ ownerId: run.run_id, generation: 1, units: 1 })]);
  });

  it("renews only with the lease's own token", async () => {
    const run = await createRun();
    const first = await createMachine(run, "hb-renew-1");
    const second = await createMachine(run, "hb-renew-2");
    const token = leaseToken("hb-renew-1");

    const renewed = await call("POST", `/v1/leases/${first.lease_id}/renew`, token, {});
    expect(renewed.status).toBe(200);
    const view = (await renewed.json()) as { heartbeat_deadline_at: string; hard_deadline_at: string };
    const due = Date.parse(view.heartbeat_deadline_at);
    expect(due).toBeGreaterThan(Date.now() + 9 * MINUTE);
    expect(due).toBeLessThanOrEqual(Date.now() + 10 * MINUTE);
    expect(view.hard_deadline_at).toBe(first.hard_deadline_at);

    // the machine may read its own lease
    expect((await readLease(first.lease_id, token)).state).toBe("running");

    // one machine's token is no credential for another lease
    await expectError(await call("POST", `/v1/leases/${second.lease_id}/renew`, token, {}), 401, "unauthorized");
    await expectError(await call("GET", `/v1/leases/${second.lease_id}`, token), 401, "unauthorized");
    await expectError(
      await call("POST", `/v1/leases/${second.lease_id}/release`, token, { reason: "done" }),
      401,
      "unauthorized",
    );
    await expectError(await call("GET", `/v1/runs/${run.run_id}`, token), 401, "unauthorized");
    expect((await readLease(second.lease_id)).state).toBe("running");

    // the client may release but not renew, and another app sees nothing
    await expectError(await call("POST", `/v1/leases/${first.lease_id}/renew`, CLIENT, {}), 403, "forbidden");
    await expectError(await call("GET", `/v1/leases/${first.lease_id}`, OTHER_CLIENT), 404, "lease_not_found");
  });

  it("deletes a machine whose heartbeat lapsed", async () => {
    const run = await createRun({ class: "disk" });
    const lease = await createMachine(run, "hb-lapse-1");
    const token = leaseToken("hb-lapse-1");
    expect(fake.volumes.size).toBe(1);

    await editHolding(lease, (holding) => {
      holding.state.lease.heartbeatDueAt = Date.now() - 1_000;
    });
    const released = await settle(lease);

    expect(released.state).toBe("released");
    expect(released.release).toMatchObject({ reason: "heartbeat_lapsed", by: "cloudlease" });
    expect(released.gone_at).not.toBeNull();
    expect(fake.droplets.size).toBe(0);
    expect(fake.log.filter((entry) => entry.startsWith("DELETE volumes/"))).toHaveLength(1);

    // the token stops working, and the permit goes back to the gate
    await expectError(await call("POST", `/v1/leases/${lease.lease_id}/renew`, token, {}), 410, "lease_ended");
    expect(await permits()).toEqual([]);
  });

  it("deletes a machine at its hard deadline", async () => {
    const run = await createRun();
    const lease = await createMachine(run, "hb-deadline-1");

    // a stored lease keeps its start before its heartbeats and its heartbeat before its deadline
    await editHolding(lease, (holding) => {
      const now = Date.now();
      holding.state.lease.startedAt = now - 3_000;
      holding.state.lease.lastHeartbeatAt = now - 3_000;
      holding.state.lease.heartbeatDueAt = now - 2_000;
      holding.state.lease.hardDeadline = now - 1_000;
    });
    const released = await settle(lease);

    expect(released.state).toBe("released");
    expect(released.release).toMatchObject({ reason: "hard_deadline", by: "cloudlease" });
    expect(fake.droplets.size).toBe(0);
  });

  it("deletes a released machine at once, and reports released only once its droplet and volume are gone", async () => {
    fake.dropletReadsAfterDelete = 2;
    const run = await createRun({ class: "disk" });
    const lease = await createMachine(run, "hb-release-1");
    const token = leaseToken("hb-release-1");
    const volumeId = [...fake.volumes.keys()][0];

    const response = await call("POST", `/v1/leases/${lease.lease_id}/release`, token, { reason: "done" });
    expect(response.status).toBe(202);
    const releasing = (await response.json()) as LeaseView;
    expect(releasing.state).toBe("releasing");
    expect(releasing.release).toMatchObject({ reason: "done", by: "machine" });

    // the release is due at once, so miniflare runs the alarm itself; the droplet is still
    // listed after its delete, so the volume must wait
    const dropletDelete = `DELETE droplets/${dropletOf("hb-release-1")?.id}`;
    await vi.waitUntil(() => fake.log.includes(dropletDelete), { timeout: 5_000 });
    expect(fake.log).not.toContain(`DELETE volumes/${volumeId}`);
    expect((await readLease(lease.lease_id)).state).toBe("releasing");
    expect(await permits()).toHaveLength(1);

    const released = await settle(lease);
    expect(released.state).toBe("released");
    expect(fake.droplets.size).toBe(0);
    expect(fake.log).toContain(`DELETE volumes/${volumeId}`);
    expect(await permits()).toEqual([]);

    // a released lease's token no longer reads it, and a second release changes nothing
    await expectError(await call("GET", `/v1/leases/${lease.lease_id}`, token), 410, "lease_ended");
    const again = await call("POST", `/v1/leases/${lease.lease_id}/release`, CLIENT, { reason: "requested" });
    expect(again.status).toBe(200);
    expect(((await again.json()) as LeaseView).release).toMatchObject({ reason: "done", by: "machine" });
  });

  it("releases every machine of a released run", async () => {
    const run = await createRun();
    const first = await createMachine(run, "hb-run-1");
    const second = await createMachine(run, "hb-run-2");

    const response = await call("POST", `/v1/runs/${run.run_id}/release`, CLIENT, { reason: "requested" });
    expect(response.status).toBe(202);
    const releasing = (await response.json()) as RunView;
    expect(releasing.state).toBe("releasing");
    expect(releasing.machines.map((machine) => machine.release)).toEqual([
      expect.objectContaining({ reason: "run_released", by: "client" }),
      expect.objectContaining({ reason: "run_released", by: "client" }),
    ]);

    await settle(first);
    await settle(second);
    expect((await readRun(run.run_id)).state).toBe("released");
    expect(fake.droplets.size).toBe(0);
    await expectError(
      await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-run-3")),
      409,
      "run_ended",
    );
  });

  it("refuses boot data too large once rendered, before admitting anything", async () => {
    const run = await createRun();
    // under the 32 KiB JSON limit, but each quote takes four bytes once shell-quoted
    const body = {
      ...machineBody("hb-big-boot"),
      boot: { files: [], run: [Array.from({ length: 8 }, () => "'".repeat(3_900))] },
    };

    await expectError(await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, body), 400, "invalid_boot");
    expect(fake.log.filter((entry) => entry.startsWith("POST"))).toEqual([]);
    expect(ntfy.sent).toEqual([]);
    expect((await readRun(run.run_id)).machines).toEqual([]);
  });

  it("takes no new machines once its class was repriced", async () => {
    const run = await createRun();
    await createMachine(run, "hb-repriced-1");
    // the deployed config now prices the class differently from when the run started
    await runInDurableObject(runStub(run.run_id), (_instance, state) => {
      const row = state.storage.sql.exec("SELECT record FROM machines_run").one();
      const record = JSON.parse(row.record as string);
      record.price_per_hour -= 1;
      state.storage.sql.exec("UPDATE machines_run SET record = ?", JSON.stringify(record));
    });

    await expectError(
      await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-repriced-2")),
      409,
      "run_ended",
    );
    expect(fake.droplets.size).toBe(1);
  });

  it("ends a lapsed lease when it is read, before its alarm runs", async () => {
    const run = await createRun();
    const lease = await createMachine(run, "hb-read-lapsed");
    const token = leaseToken("hb-read-lapsed");

    await editHolding(lease, (holding) => {
      holding.state.lease.heartbeatDueAt = Date.now() - 1_000;
    });

    await expectError(await call("GET", `/v1/leases/${lease.lease_id}`, token), 410, "lease_ended");
    const view = await readLease(lease.lease_id);
    expect(view.state).not.toBe("running");
    expect(view.release).toMatchObject({ reason: "heartbeat_lapsed", by: "cloudlease" });
  });

  it("refuses a machine past max_machines", async () => {
    const run = await createRun({ max_machines: 1 });
    await createMachine(run, "hb-full-1");

    await expectError(
      await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-full-2")),
      409,
  it("never gives an expired pending create's generation to a later request", async () => {
    const run = await createRun();
    const stub = runStub(run.run_id);
    let openGate!: () => void;
    const gateClosed = new Promise<void>((resolve) => {
      openGate = resolve;
    });
    const stall = runInDurableObject(gateStub(), (_instance, state) => state.blockConcurrencyWhile(() => gateClosed));

    // the first create waits on the gate until its pending record has expired
    const first = call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-expired-1"));
    await vi.waitUntil(
      () =>
        runInDurableObject(stub, (_instance, state) => {
          return state.storage.sql.exec("SELECT generation FROM machines_requests").toArray().length === 1;
        }),
      { timeout: 5_000, interval: 20 },
    );
    await runInDurableObject(stub, (_instance, state) => {
      const row = state.storage.sql.exec("SELECT record FROM machines_requests").one();
      const record = JSON.parse(row.record as string);
      record.admission.since -= 11 * MINUTE;
      state.storage.sql.exec("UPDATE machines_requests SET record = ?", JSON.stringify(record));
    });
    await runDurableObjectAlarm(stub);
    expect(await runInDurableObject(stub, (_instance, state) => state.storage.sql.exec("SELECT * FROM machines_requests").toArray())).toEqual([]);

    // a second create takes a new generation, so the first create's late gate reply cannot
    // admit it with the first request's boot data
    const second = call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-expired-2"));
    await vi.waitUntil(
      () =>
        runInDurableObject(stub, (_instance, state) => {
          return state.storage.sql.exec("SELECT generation FROM machines_requests").toArray().length === 1;
        }),
      { timeout: 5_000, interval: 20 },
    );
    openGate();
    await stall;

    await expectError(await first, 503, "busy");
    const response = await second;
    expect(response.status).toBe(201);
    const lease = (await response.json()) as LeaseView;
    expect(lease.hostname).toBe("hb-expired-2");
    expect(generationOf(lease.lease_id)).toBe(2);
    expect([...fake.droplets.values()].map((droplet) => droplet.name)).toEqual(["hb-expired-2"]);
    // the late reply gave its permit back
    expect((await permits()).map((permit) => permit.generation)).toEqual([2]);
  }, 20_000);

      "run_full",
    );
    expect(fake.droplets.size).toBe(1);
  });

  it("refuses a machine the hourly spend cap cannot pay for, and alerts", async () => {
    const run = await createRun({ class: "huge" });
    const body = machineBody("hb-hourly-1");

    await expectError(await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, body), 429, "spend_cap_reached");
    expect(fake.log.filter((entry) => entry.startsWith("POST"))).toEqual([]);
    expect(ntfy.titles()).toEqual(["cloudlease: hourly spend cap reached"]);
    expect(ntfy.sent[0]).toMatchObject({ priority: "5", authorization: "Bearer test-ntfy-token" });

    // the refusal holds no slot of the run, so a retry is refused again rather than busy
    await expectError(await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, body), 429, "spend_cap_reached");
    expect((await readRun(run.run_id)).machines).toEqual([]);
  });

  it("refuses a machine once the run has spent its cap", async () => {
    const run = await createRun();
    const lease = await createMachine(run, "hb-capped-1");

    // six hours at $0.01 is more than the $0.05 cap
    await backdateMachine(lease, 6 * HOUR);
    await expectError(
      await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-capped-2")),
      429,
      "spend_cap_reached",
    );
    expect(fake.droplets.size).toBe(1);
  });

  it("holds a run to the client's spend cap, which may not pass the config's", async () => {
    await expectError(await call("POST", "/v1/runs", CLIENT, runBody({ spend_cap_cents: 6 })), 400, "invalid_request");
    await expectError(await call("POST", "/v1/runs", CLIENT, runBody({ spend_cap_cents: 0 })), 400, "invalid_request");

    const body = runBody({ spend_cap_cents: 2 });
    const created = await call("POST", "/v1/runs", CLIENT, body);
    expect(created.status).toBe(201);
    const run = (await created.json()) as RunView;
    expect((await call("POST", "/v1/runs", CLIENT, body)).status).toBe(200);
    await expectError(
      await call("POST", "/v1/runs", CLIENT, { ...body, spend_cap_cents: 3 }),
      409,
      "request_conflict",
    );
    await expectError(await call("POST", "/v1/runs", CLIENT, { ...body, spend_cap_cents: undefined }), 409, "request_conflict");

    // three hours at $0.01 is past the run's 2 cent cap but inside the config's 5 cents
    const lease = await createMachine(run, "hb-client-cap-1");
    await backdateMachine(lease, 3 * HOUR);
    await expectError(
      await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-client-cap-2")),
      429,
      "spend_cap_reached",
    );
  });

  it("releases a run's machines when it reaches its spend cap, and alerts", async () => {
    const run = await createRun();
    const lease = await createMachine(run, "hb-spend-1");

    await backdateMachine(lease, 6 * HOUR);
    const released = await settle(lease);

    expect(released.state).toBe("released");
    expect(released.release).toMatchObject({ reason: "spend_cap", by: "cloudlease" });
    expect(fake.droplets.size).toBe(0);
    expect((await readRun(run.run_id)).state).toBe("released");
    expect(ntfy.titles()).toEqual([`cloudlease: run pool-test reached its spend cap`]);
    expect(ntfy.sent[0]?.body).toContain(run.run_id);
  });

  it("ends a lease whose create failed, and alerts once per run", async () => {
    const run = await createRun();
    fake.failCreates = true;

    const first = await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-fail-1"));
    expect(first.status).toBe(201);
    const lease = (await first.json()) as LeaseView;
    expect(lease.release).toMatchObject({ reason: "create_failed", by: "cloudlease" });
    expect(["releasing", "released"]).toContain(lease.state);

    const second = await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-fail-2"));
    expect(second.status).toBe(201);
    expect(ntfy.titles()).toEqual([`cloudlease: machine create failed in run pool-test`]);

    // an empty tag search right after a lease ends may miss a create still in flight, so the
    // search only confirms absence once the release is old enough
    fake.failCreates = false;
    await editHolding(lease, (holding) => {
      holding.state.since -= HOUR;
    });
    expect((await settle(lease)).state).toBe("released");
  });

  it("sweeps a tagged droplet no run claims, and alerts on it as an orphan", async () => {
    const ghost = "run_000000000000000000000000";
    fake.addDroplet(7001, tags(ghost, 1, "droplet", undefined, "clm"));
    const run = await createRun();
    await createMachine(run, "hb-sweep-1");

    await scheduled();

    expect(fake.log).toContain("DELETE droplets/7001");
    expect(dropletOf("hb-sweep-1")).toBeDefined();
    expect(ntfy.titles()).toEqual(["cloudlease: sweep deleted 1 orphaned resources"]);
    expect(ntfy.sent[0]?.body).toContain("7001");
  });

  it("does not alert on a resource its run is already releasing", async () => {
    fake.dropletReadsAfterDelete = 5;
    const run = await createRun();
    const lease = await createMachine(run, "hb-sweep-releasing");
    const dropletId = dropletOf("hb-sweep-releasing")?.id;
    const response = await call("POST", `/v1/leases/${lease.lease_id}/release`, CLIENT, { reason: "idle" });
    expect(response.status).toBe(202);

    await scheduled();

    expect(fake.log).toContain(`DELETE droplets/${dropletId}`);
    expect(ntfy.titles()).toEqual([]);
  });

  it("deletes a stalled run's droplet past its hard deadline on the next sweep", async () => {
    const run = await createRun();
    const past = Math.floor(Date.now() / 1000) - 10;
    fake.addDroplet(7009, tags(run.run_id, 9, "droplet", past, "clm"));
    // the run object takes no events for longer than the sweep's 5 second owner timeout
    const stall = runInDurableObject(runStub(run.run_id), (_instance, state) =>
      state.blockConcurrencyWhile(() => new Promise((resolve) => setTimeout(resolve, 7_000))),
    );

    await scheduled();
    await stall;

    expect(fake.log).toContain("DELETE droplets/7009");
    expect(fake.droplets.has(7009)).toBe(false);
  }, 30_000);

  it("alerts when the sweep keeps failing", async () => {
    fake.failLists = true;

    await scheduled();
    await scheduled();
    expect(ntfy.titles()).toEqual([]);
    await scheduled();

    expect(ntfy.titles()).toEqual(["cloudlease: the machines sweep keeps failing"]);

    // a good sweep resets the streak
    fake.failLists = false;
    await scheduled();
    fake.failLists = true;
    await scheduled();
    expect(ntfy.titles()).toHaveLength(1);
  });
});
