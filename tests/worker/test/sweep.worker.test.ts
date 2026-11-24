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
// scheduled controllers cannot cross the loopback binding, so the suite calls the shim directly
// @ts-expect-error worker-build emits no declaration for its JavaScript shim
import RustWorker from "../build/index.js";
import { FakeDigitalOcean, tags } from "./fake-digitalocean";

interface DeleteReport {
  resource: { id: string; tags: { owner: string; generation: number; kind: string } };
  reason: "pastCeiling" | "unclaimed";
  outcome: { outcome: "gone" | "stillPresent" | "deferred" | "failed" };
}

interface SweepReport {
  managed: number;
  unrecognized: Array<{ id: string; reason: string }>;
  ownersAsked: number;
  ownersFailed: Array<{ owner: string; error: string }>;
  deletes: DeleteReport[];
  releasedPermits: string[];
  capacityErrors: string[];
}

interface Permit {
  permitId: string;
  ownerId: string;
  generation: number;
  expiresAt: number;
  status: "active" | "suspect";
}

type Slot = { status: "requested" } | { status: "created"; id: string };

const MINUTE = 60_000;

let fake: FakeDigitalOcean;

function ownerStub(owner: string): DurableObjectStub {
  return env.OWNERS.get(env.OWNERS.idFromName(owner));
}

function capacityStub(): DurableObjectStub {
  return env.CAPACITY.get(env.CAPACITY.idFromName("global"));
}

async function sweep(): Promise<SweepReport> {
  const response = await SELF.fetch("https://worker.test/test/sweep", { method: "POST" });
  expect(response.status).toBe(200);
  return response.json();
}

function leasedHolding(
  owner: string,
  generation: number,
  times: { startedAt: number; heartbeatDueAt: number; hardDeadline: number },
  slots: Record<string, Slot>,
) {
  return {
    owner,
    generation,
    state: {
      state: "leased",
      lease: {
        id: `lease_${owner}`,
        startedAt: times.startedAt,
        lastHeartbeatAt: times.startedAt,
        lastSequence: 0,
        heartbeatDueAt: times.heartbeatDueAt,
        hardDeadline: times.hardDeadline,
      },
      slots,
    },
  };
}

// seeding arms no alarm, so in the sweep tests only the sweep can delete anything
async function seedHolding(owner: string, holding: object): Promise<void> {
  const response = await ownerStub(owner).fetch("https://owner/holdings", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(holding),
  });
  expect(response.status).toBe(201);
}

// parks the owner's alarm an hour out, so miniflare cannot fire it on its own and only
// runDurableObjectAlarm runs it; returns the alarm time it replaced
async function parkAlarm(stub: DurableObjectStub): Promise<number | null> {
  return runInDurableObject(stub, async (_instance, state) => {
    const previous = await state.storage.getAlarm();
    await state.storage.setAlarm(Date.now() + 60 * MINUTE);
    return previous;
  });
}

async function leaseOwner(owner: string, generation: number): Promise<void> {
  const response = await ownerStub(owner).fetch("https://owner/lease", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ ownerId: owner, generation, leaseId: `lease_${owner}` }),
  });
  expect(response.status).toBe(201);
}

async function holdings(
  owner: string,
): Promise<Array<{ state: { state: string; lease?: { heartbeatDueAt: number } } }>> {
  const response = await ownerStub(owner).fetch("https://owner/holdings");
  expect(response.status).toBe(200);
  return response.json();
}

async function acquirePermit(ownerId: string, generation: number, ttlMs: number) {
  return capacityStub().fetch("https://capacity/cloudlease/capacity/acquire", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ ownerId, generation, ttlMs }),
  });
}

async function permits(): Promise<Permit[]> {
  const response = await capacityStub().fetch("https://capacity/cloudlease/capacity/snapshot");
  expect(response.status).toBe(200);
  const snapshot = (await response.json()) as { permits: Permit[] };
  return snapshot.permits;
}

function outcomes(report: SweepReport): Array<[string, string, string]> {
  return report.deletes.map((entry) => [entry.resource.id, entry.reason, entry.outcome.outcome]);
}

describe("cloudlease sweep in miniflare", () => {
  beforeEach(async () => {
    fake = new FakeDigitalOcean();
    fake.install();

    // the gate creates its tables on first use, so read it before clearing it
    await permits();
    await runInDurableObject(capacityStub(), (_instance, state) => {
      state.storage.sql.exec("DELETE FROM cloudlease_permits");
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("deletes a tagged droplet and volume whose owner object never existed", async () => {
    fake.addDroplet(301, tags("ghost_owner", 1, "droplet"));
    fake.addVolume("vol-301", tags("ghost_owner", 1, "volume"), [301]);

    // another owner is mid-create: the droplet exists and is tagged, but its ID is not
    // recorded yet, so only the intent saved before the create call protects it
    await leaseOwner("job_creating", 1);
    const release = fake.hold("POST");
    const creating = ownerStub("job_creating").fetch("https://owner/droplet", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ generation: 1 }),
    });
    const inFlight = await vi.waitUntil(() =>
      [...fake.droplets.values()].find((droplet) => droplet.tags.includes("cl-owner-job_creating")),
    );
    expect(inFlight.tags).toContain("cl-gen-1");
    expect(inFlight.tags.some((tag) => tag.startsWith("cl-until-"))).toBe(true);

    const report = await sweep();

    expect(report.managed).toBe(3);
    expect(report.ownersAsked).toBe(2);
    expect(report.ownersFailed).toEqual([]);
    expect(outcomes(report)).toEqual([
      ["301", "unclaimed", "gone"],
      ["vol-301", "unclaimed", "gone"],
    ]);
    expect(fake.droplets.has(inFlight.id)).toBe(true);

    release();
    const created = await creating;
    expect(created.status).toBe(201);
    expect(await created.json()).toMatchObject({ id: String(inFlight.id) });
    expect(outcomes(await sweep())).toEqual([]);
    expect(fake.droplets.has(inFlight.id)).toBe(true);
    // the droplet is confirmed gone before its attached volume is deleted
    expect(fake.deletesOf(301, "vol-301")).toEqual([
      "DELETE droplets/301",
      "DELETE volumes/vol-301",
    ]);
    expect(fake.log).toContain("GET droplets/301");
    expect(fake.log).toContain("GET volumes/vol-301");
    expect([...fake.droplets.keys()]).toEqual([inFlight.id]);
    expect(fake.volumes.size).toBe(0);
    await expect(holdings("ghost_owner")).resolves.toEqual([]);
  });

  it("deletes an owned droplet whose lease is past its hard deadline", async () => {
    const now = Date.now();
    await seedHolding(
      "job_expired",
      leasedHolding(
        "job_expired",
        1,
        { startedAt: now - 70 * MINUTE, heartbeatDueAt: now - 10 * MINUTE, hardDeadline: now - MINUTE },
        { droplet: { status: "created", id: "401" } },
      ),
    );
    fake.addDroplet(401, tags("job_expired", 1, "droplet"));

    const report = await sweep();

    expect(outcomes(report)).toEqual([["401", "unclaimed", "gone"]]);
    expect(fake.droplets.size).toBe(0);
  });

  it("leaves a live owned droplet and its pending volume alone but deletes a duplicate", async () => {
    const now = Date.now();
    await seedHolding(
      "job_live",
      leasedHolding(
        "job_live",
        1,
        { startedAt: now, heartbeatDueAt: now + 10 * MINUTE, hardDeadline: now + 60 * MINUTE },
        { droplet: { status: "created", id: "501" }, volume: { status: "requested" } },
      ),
    );
    fake.addDroplet(501, tags("job_live", 1, "droplet"));
    fake.addVolume("vol-501", tags("job_live", 1, "volume"), [501]);
    // a second droplet for the same generation that the lease never recorded
    fake.addDroplet(502, tags("job_live", 1, "droplet"));

    const report = await sweep();

    expect(outcomes(report)).toEqual([["502", "unclaimed", "gone"]]);
    expect([...fake.droplets.keys()]).toEqual([501]);
    expect([...fake.volumes.keys()]).toEqual(["vol-501"]);
  });

  it("deletes a claimed droplet past its ceiling without asking the owner", async () => {
    const now = Date.now();
    await seedHolding(
      "job_ceiling",
      leasedHolding(
        "job_ceiling",
        1,
        { startedAt: now, heartbeatDueAt: now + 10 * MINUTE, hardDeadline: now + 60 * MINUTE },
        { droplet: { status: "created", id: "601" } },
      ),
    );
    fake.addDroplet(601, tags("job_ceiling", 1, "droplet", Math.floor(now / 1000) - 10));

    const report = await sweep();

    expect(report.ownersAsked).toBe(0);
    expect(outcomes(report)).toEqual([["601", "pastCeiling", "gone"]]);
  });

  it("asks the owner about a droplet whose ceiling tag is malformed", async () => {
    const now = Date.now();
    await seedHolding(
      "job_badceiling",
      leasedHolding(
        "job_badceiling",
        1,
        { startedAt: now, heartbeatDueAt: now + 10 * MINUTE, hardDeadline: now + 60 * MINUTE },
        { droplet: { status: "created", id: "651" } },
      ),
    );
    const malformed = (owner: string) => [...tags(owner, 1, "droplet"), "cl-until-soon"];
    fake.addDroplet(651, malformed("job_badceiling"));
    fake.addDroplet(652, malformed("ghost_badceiling"));

    const report = await sweep();

    // neither is read as past its ceiling or as having none: the live owner keeps its
    // droplet and the owner that never existed loses its own
    expect(report.unrecognized).toEqual([]);
    expect(report.ownersAsked).toBe(2);
    expect(outcomes(report)).toEqual([["652", "unclaimed", "gone"]]);
    expect([...fake.droplets.keys()]).toEqual([651]);
  });

  it("keeps the resources of an owner that cannot answer", async () => {
    fake.addDroplet(701, tags("job_broken", 1, "droplet"));
    const stub = ownerStub("job_broken");
    await holdings("job_broken");
    await runInDurableObject(stub, (_instance, state) => {
      state.storage.sql.exec(
        "INSERT INTO cloudlease_holdings (generation, holding, updated_at) VALUES (1, 'not json', 0)",
      );
    });

    const report = await sweep();

    expect(report.ownersFailed.map((failure) => failure.owner)).toEqual(["job_broken"]);
    expect(report.deletes).toEqual([]);
    expect(fake.droplets.has(701)).toBe(true);
  });

  it("leaves resources with an unrecognized tag shape alone", async () => {
    fake.addDroplet(801, ["cl-managed", "team-x"]);
    fake.addDroplet(802, ["rb-managed", "rb-project-other"]);

    const report = await sweep();

    expect(report.unrecognized.map((entry) => [entry.id, entry.reason])).toEqual([
      ["801", "extraTag"],
    ]);
    expect(report.deletes).toEqual([]);
    expect(fake.droplets.size).toBe(2);
  });

  it("releases a suspect permit once the sweep proves its resources absent", async () => {
    expect((await acquirePermit("ghost_permit", 1, 1)).status).toBe(201);
    expect((await acquirePermit("unrecognized_permit", 1, 1)).status).toBe(201);
    await new Promise((resolve) => setTimeout(resolve, 5));
    fake.addDroplet(901, tags("ghost_permit", 1, "droplet"));

    const clean = await sweep();

    expect(outcomes(clean)).toEqual([["901", "unclaimed", "gone"]]);
    expect(clean.releasedPermits).toHaveLength(2);
    await expect(permits()).resolves.toEqual([]);

    // with an unrecognized resource in the account, absence cannot be proven
    expect((await acquirePermit("ghost_permit", 2, 1)).status).toBe(201);
    await new Promise((resolve) => setTimeout(resolve, 5));
    fake.addDroplet(902, ["cl-managed", "team-x"]);

    const incomplete = await sweep();

    expect(incomplete.releasedPermits).toEqual([]);
    expect((await permits()).map((permit) => permit.ownerId)).toEqual(["ghost_permit"]);
  });

  it("counts suspect permits against the limit", async () => {
    expect((await acquirePermit("job_a", 1, 60_000)).status).toBe(201);
    expect((await acquirePermit("job_a", 1, 60_000)).status).toBe(200);
    expect((await acquirePermit("job_b", 1, 1)).status).toBe(201);
    await new Promise((resolve) => setTimeout(resolve, 5));
    await runDurableObjectAlarm(capacityStub());

    const exhausted = await acquirePermit("job_c", 1, 60_000);
    expect(exhausted.status).toBe(409);
    expect((await permits()).map((permit) => [permit.ownerId, permit.status])).toEqual([
      ["job_b", "suspect"],
      ["job_a", "active"],
    ]);

    const release = await capacityStub().fetch("https://capacity/cloudlease/capacity/release", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ permitId: "cap_x", ownerId: "job_b", generation: 1, ownerAbsent: false }),
    });
    expect(release.status).toBe(400);
  });

  it("an owner alarm deletes and confirms the resources of an expired lease", async () => {
    const now = Date.now();
    await seedHolding(
      "job_alarm",
      leasedHolding(
        "job_alarm",
        1,
        { startedAt: now - 5 * MINUTE, heartbeatDueAt: now - MINUTE, hardDeadline: now + 55 * MINUTE },
        { droplet: { status: "created", id: "1001" }, volume: { status: "requested" } },
      ),
    );
    fake.addDroplet(1001, tags("job_alarm", 1, "droplet"));
    // the volume's create response was lost, so only its tags lead to it
    fake.addVolume("vol-1001", tags("job_alarm", 1, "volume"), [1001]);
    const stub = ownerStub("job_alarm");
    await parkAlarm(stub);

    // the first pass deletes the droplet, which is still visible once, and retries a second later
    expect(await runDurableObjectAlarm(stub)).toBe(true);
    const retryAt = await parkAlarm(stub);
    expect(fake.deletesOf(1001, "vol-1001")).toEqual(["DELETE droplets/1001"]);
    expect((await holdings("job_alarm"))[0]?.state.state).toBe("releasing");
    expect(retryAt).toBeGreaterThan(now);
    expect(retryAt).toBeLessThan(Date.now() + 2_000);

    await new Promise((resolve) => setTimeout(resolve, 1_100));
    expect(await runDurableObjectAlarm(stub)).toBe(true);

    expect(fake.deletesOf(1001, "vol-1001")).toEqual([
      "DELETE droplets/1001",
      "DELETE droplets/1001",
      "DELETE volumes/vol-1001",
    ]);
    expect(fake.droplets.size).toBe(0);
    expect(fake.volumes.size).toBe(0);
    expect((await holdings("job_alarm"))[0]?.state.state).toBe("released");
  });

  it("an owner alarm keeps the alarm of a lease saved while it was deleting", async () => {
    const now = Date.now();
    await seedHolding(
      "job_race",
      leasedHolding(
        "job_race",
        1,
        { startedAt: now - 5 * MINUTE, heartbeatDueAt: now - MINUTE, hardDeadline: now + 55 * MINUTE },
        { droplet: { status: "created", id: "1201" } },
      ),
    );
    fake.addDroplet(1201, tags("job_race", 1, "droplet"));
    fake.dropletReadsAfterDelete = 0;
    const stub = ownerStub("job_race");
    await parkAlarm(stub);

    // a new lease is saved while the alarm waits on the provider; the alarm pass must not
    // re-arm from the holdings it read before that
    const release = fake.hold("DELETE");
    const alarmPass = runDurableObjectAlarm(stub);
    await vi.waitUntil(() => fake.log.includes("DELETE droplets/1201"));
    await leaseOwner("job_race", 2);
    release();
    expect(await alarmPass).toBe(true);

    const states = await holdings("job_race");
    expect(states.map((holding) => holding.state.state)).toEqual(["released", "leased"]);
    const alarm = await runInDurableObject(stub, (_instance, state) => state.storage.getAlarm());
    expect(alarm).toBe(states[1]?.state.lease?.heartbeatDueAt);
  });

  it("sweeps despite a volume whose tags and attachments are null", async () => {
    fake.addDroplet(1301, tags("ghost_nulls", 1, "droplet"));
    fake.addVolume("vol-unrelated", null, null);

    const report = await sweep();

    expect(outcomes(report)).toEqual([["1301", "unclaimed", "gone"]]);
    expect(fake.volumes.has("vol-unrelated")).toBe(true);
  });

  it("runs the sweep from the cron trigger", async () => {
    fake.addDroplet(1101, tags("ghost_cron", 3, "droplet"));
    const controller = createScheduledController({
      cron: "*/5 * * * *",
      scheduledTime: new Date(),
    });
    const context = createExecutionContext();
    const worker = new RustWorker(context, env);

    await worker.scheduled(controller);
    await waitOnExecutionContext(context);

    expect(fake.deletesOf(1101)).toEqual(["DELETE droplets/1101"]);
    expect(fake.droplets.size).toBe(0);
  });
});
