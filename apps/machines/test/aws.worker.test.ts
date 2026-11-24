import { runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeDigitalOcean } from "../../../tests/worker/test/fake-digitalocean";
import { FakeEc2, type Instance, instanceTags } from "./fake-ec2";
import { FakeLatitude } from "./fake-latitude";
import { FakeNtfy } from "./fake-ntfy";
import { FakePhoenixNap } from "./fake-phoenixnap";
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
  runStub,
  scheduled,
  settle,
} from "./support";

let ec2: FakeEc2;
let ntfy: FakeNtfy;

function instanceOf(hostname: string): Instance {
  const instance = ec2.named(hostname);
  expect(instance).toBeDefined();
  return instance as Instance;
}

// the lease file reaches the instance only inside its user data
function leaseToken(hostname: string): string {
  const match = /cloudlease_write '\/etc\/cloudlease\/lease\.json' 0600 '([A-Za-z0-9+/=]+)'/.exec(
    instanceOf(hostname).userData,
  );
  expect(match).not.toBeNull();
  return JSON.parse(atob(match?.[1] ?? "")).token;
}

async function alarmAt(runId: string): Promise<number | null> {
  return runInDurableObject(runStub(runId), (_instance, state) => state.storage.getAlarm());
}

async function spotRun(overrides: Record<string, unknown> = {}) {
  return createRun({ class: "spot", name: "pool-spot", ...overrides });
}

describe("AWS Spot machines in miniflare", () => {
  beforeEach(async () => {
    // the sweep lists the other providers too
    new FakeDigitalOcean().install();
    new FakePhoenixNap().install();
    new FakeLatitude().install();
    ec2 = new FakeEc2();
    ec2.install();
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

  it("launches a one-time Spot instance with the class's first type and zone, its tags and boot data", async () => {
    const run = await spotRun();
    const lease = await createMachine(run, "hb-spot-1");

    const instance = instanceOf("hb-spot-1");
    expect(instance).toMatchObject({
      instanceType: "c8a.4xlarge",
      zone: "us-east-1a",
      subnetId: "subnet-0123456789abcdef0",
      imageId: "ami-0123456789abcdef0",
      securityGroupId: "sg-0123456789abcdef0",
      rootDevice: "/dev/sda1",
      rootVolumeGib: 150,
      // terminated on interruption, so EC2 never relaunches a machine whose lease ended
      spotType: "one-time",
      interruptionBehavior: "terminate",
      maxPrice: "0.025",
    });
    const until = Math.floor(Date.parse(lease.hard_deadline_at) / 1000);
    expect(instance.tags).toEqual([...instanceTags(run.run_id, 1, until), { key: "Name", value: "hb-spot-1" }]);
    expect(ec2.spotRequests.get(instance.spotRequestId ?? "")?.tags).toEqual(instance.tags);
    expect(instance.userData).toMatch(/^#!\/bin\/sh\n/);
    expect(leaseToken("hb-spot-1")).toMatch(/^cll_[0-9a-f]{64}$/);

    // an 8 minute boot grace, a 3 cent permit, and a wake to check the instance after 2 minutes
    expect(lease.state).toBe("running");
    expect(Date.parse(lease.heartbeat_deadline_at) - Date.parse(lease.created_at)).toBe(8 * MINUTE);
    expect(await permits()).toEqual([expect.objectContaining({ ownerId: run.run_id, generation: 1, units: 3 })]);
    const wake = await alarmAt(run.run_id);
    expect(wake).not.toBeNull();
    expect((wake ?? 0) - Date.parse(lease.created_at)).toBeLessThanOrEqual(2 * MINUTE + 5_000);
  });

  it("tries each instance type in every zone on a capacity error, and fails the create after the last", async () => {
    ec2.noCapacity.add("c8a.4xlarge@us-east-1a");
    const run = await spotRun();

    await createMachine(run, "hb-spot-capacity-1");
    expect(instanceOf("hb-spot-capacity-1")).toMatchObject({ instanceType: "c8a.4xlarge", zone: "us-east-1b" });
    expect(ec2.launches()).toEqual(["RunInstances c8a.4xlarge us-east-1a", "RunInstances c8a.4xlarge us-east-1b"]);

    // the dearer type is tried only once the cheaper one has no capacity in any zone
    ec2.noCapacity.add("c8a.4xlarge");
    ec2.noCapacity.add("us-east-1a");
    ec2.log.length = 0;
    await createMachine(run, "hb-spot-capacity-2");
    expect(instanceOf("hb-spot-capacity-2")).toMatchObject({ instanceType: "c7a.4xlarge", zone: "us-east-1b" });
    expect(ec2.launches()).toEqual([
      "RunInstances c8a.4xlarge us-east-1a",
      "RunInstances c8a.4xlarge us-east-1b",
      "RunInstances c7a.4xlarge us-east-1a",
      "RunInstances c7a.4xlarge us-east-1b",
    ]);

    ec2.noCapacity.add("c7a.4xlarge");
    const response = await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-spot-capacity-3"));
    expect(response.status).toBe(201);
    const failed = (await response.json()) as LeaseView;
    expect(failed.release).toMatchObject({ reason: "create_failed", by: "cloudlease" });
    expect(ec2.named("hb-spot-capacity-3")).toBeUndefined();
    expect(ntfy.titles()).toEqual(["cloudlease: machine create failed in run pool-spot"]);
    expect(ntfy.sent[0]?.body).toContain("no Spot capacity under the price cap");

    // EC2 launched nothing, so the refused create's permit is back at once
    const held = (await permits()).filter((permit) => permit.ownerId === run.run_id).map((permit) => permit.generation);
    expect(held.sort()).toEqual([1, 2]);
  });

  it("renews, and terminates a released instance and confirms it gone", async () => {
    const run = await spotRun();
    const lease = await createMachine(run, "hb-spot-release");
    const token = leaseToken("hb-spot-release");
    const id = instanceOf("hb-spot-release").id;

    expect((await call("POST", `/v1/leases/${lease.lease_id}/renew`, token, {})).status).toBe(200);
    expect((await call("POST", `/v1/leases/${lease.lease_id}/release`, token, { reason: "done" })).status).toBe(202);

    const released = await settle(lease);
    expect(released.state).toBe("released");
    expect(released.release).toMatchObject({ reason: "done", by: "machine" });
    expect(ec2.terminationsOf(id)).toHaveLength(1);
    expect(ec2.instances.get(id)?.state).toBe("terminated");
    expect(await permits()).toEqual([]);
  });

  it("terminates an instance whose heartbeat lapsed", async () => {
    const run = await spotRun();
    const lease = await createMachine(run, "hb-spot-lapse");
    const id = instanceOf("hb-spot-lapse").id;

    await editHolding(lease, (holding) => {
      holding.state.lease.heartbeatDueAt = Date.now() - 1_000;
    });
    const released = await settle(lease);

    expect(released.state).toBe("released");
    expect(released.release).toMatchObject({ reason: "heartbeat_lapsed", by: "cloudlease" });
    expect(ec2.terminationsOf(id)).toHaveLength(1);
    expect(ec2.instances.get(id)?.state).toBe("terminated");
  });

  it("terminates an instance at its hard deadline", async () => {
    const run = await spotRun();
    const lease = await createMachine(run, "hb-spot-deadline");
    const id = instanceOf("hb-spot-deadline").id;

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
    expect(ec2.instances.get(id)?.state).toBe("terminated");
  });

  it("ends the lease of an interrupted instance as interrupted and returns its permit", async () => {
    const run = await spotRun();
    const lease = await createMachine(run, "hb-spot-interrupt");
    const token = leaseToken("hb-spot-interrupt");
    const id = instanceOf("hb-spot-interrupt").id;
    ec2.interrupt(id);

    // inside the first two minutes a missing instance may only be EC2's eventual consistency
    await runDurableObjectAlarm(runStub(run.run_id));
    expect((await readLease(lease.lease_id)).state).toBe("running");

    await backdateMachine(lease, 3 * MINUTE);
    await runDurableObjectAlarm(runStub(run.run_id));
    const released = await settle(lease);

    expect(released.state).toBe("released");
    expect(released.release).toMatchObject({ reason: "interrupted", by: "cloudlease" });
    expect(released.gone_at).not.toBeNull();
    // EC2 already terminated it and closed its one-time request, so nothing is sent
    expect(ec2.terminationsOf(id)).toEqual([]);
    expect(ec2.log.filter((entry) => entry.startsWith("CancelSpotInstanceRequests"))).toEqual([]);
    expect(await permits()).toEqual([]);
    // the machine's own token stops working with the lease
    await expectError(await call("POST", `/v1/leases/${lease.lease_id}/renew`, token, {}), 410, "lease_ended");
  });

  it("keeps the reason of a release that lands while a presence check is in flight", async () => {
    const run = await spotRun();
    const lease = await createMachine(run, "hb-spot-race");
    const token = leaseToken("hb-spot-race");
    const id = instanceOf("hb-spot-race").id;
    await backdateMachine(lease, 3 * MINUTE);

    // the machine finishes and releases its lease, and EC2 terminates the instance, while the
    // alarm waits on its DescribeInstances
    let release: Response | undefined;
    ec2.beforeAnswer = async (action) => {
      expect(action).toBe("DescribeInstances");
      release = await call("POST", `/v1/leases/${lease.lease_id}/release`, token, { reason: "done" });
      ec2.interrupt(id);
    };
    await runDurableObjectAlarm(runStub(run.run_id));
    expect(release?.status).toBe(202);

    const released = await settle(lease);
    expect(released.release).toMatchObject({ reason: "done", by: "machine" });
    expect(await permits()).toEqual([]);
  });

  it("keeps a running instance's lease across presence checks", async () => {
    const run = await spotRun();
    const lease = await createMachine(run, "hb-spot-present");

    await backdateMachine(lease, 3 * MINUTE);
    await runDurableObjectAlarm(runStub(run.run_id));

    expect((await readLease(lease.lease_id)).state).toBe("running");
    expect(ec2.log).toContain(`DescribeInstances ${instanceOf("hb-spot-present").id}`);
    // the next check is about a minute away
    const wake = (await alarmAt(run.run_id)) ?? 0;
    expect(wake - Date.now()).toBeGreaterThan(50_000);
    expect(wake - Date.now()).toBeLessThanOrEqual(60_000);
  });

  it("sweeps an unclaimed instance past its ceiling and keeps a claimed one", async () => {
    const run = await spotRun();
    await createMachine(run, "hb-spot-sweep");
    const claimed = instanceOf("hb-spot-sweep").id;
    const past = Math.floor(Date.now() / 1000) - 10;
    const ghost = ec2.addInstance([...instanceTags("run_000000000000000000000000", 1, past), { key: "Name", value: "ghost" }]);
    // a near miss is never managed, so it is reported but left alone
    const foreign = ec2.addInstance([
      { key: "clm-managed", value: "true" },
      { key: "clm-owner", value: run.run_id },
    ]);
    // another app's instance is not listed at all
    const rembuild = ec2.addInstance(instanceTags("prj_1", 1, past, "rb"));

    await scheduled();

    expect(ec2.terminationsOf(ghost)).toHaveLength(1);
    expect(ec2.instances.get(ghost)?.state).toBe("terminated");
    expect(ec2.instances.get(claimed)?.state).toBe("pending");
    expect(ec2.instances.get(foreign)?.state).toBe("running");
    expect(ec2.instances.get(rembuild)?.state).toBe("running");
    expect(ntfy.titles()).toContain("cloudlease: sweep deleted 1 orphaned resources");
  });

  it("bills per second with a 60 second minimum: half an hour costs half an hour", async () => {
    // a 2 cent cap holds about 40 minutes of a $0.03 instance
    const run = await spotRun({ spend_cap_cents: 2 });
    const lease = await createMachine(run, "hb-spot-spend-1");

    // after 30 minutes the instance has cost $0.015, so another fits; billed by the
    // started hour it would already have cost $0.03
    await backdateMachine(lease, 30 * MINUTE);
    await createMachine(run, "hb-spot-spend-2");

    // at 42 minutes it has cost $0.021, past the cap with the second's minute
    await backdateMachine(lease, 12 * MINUTE);
    await expectError(
      await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody("hb-spot-spend-3")),
      429,
      "spend_cap_reached",
    );
    expect(ec2.launches()).toHaveLength(2);
  });

  it("refuses boot data that fits a droplet but not EC2's 16 KiB of user data", async () => {
    const run = await spotRun();
    // about 17 KB of script, which a DigitalOcean class takes
    const body = { ...machineBody("hb-spot-big"), boot: { files: [], run: [["x".repeat(17_000)]] } };

    await expectError(await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, body), 400, "invalid_boot");
    expect(ec2.launches()).toEqual([]);
    expect(await permits()).toEqual([]);
  });

  it("reports aws on health", async () => {
    const response = await call("GET", "/v1/health", null);
    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({ ok: true, aws: true });
  });
});
