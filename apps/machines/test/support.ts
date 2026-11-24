// helpers the machines suites share: calls to the Worker, run and lease views, and edits
// of a run object's stored state

import {
  createExecutionContext,
  createScheduledController,
  runDurableObjectAlarm,
  runInDurableObject,
  SELF,
  waitOnExecutionContext,
} from "cloudflare:test";
import { env } from "cloudflare:workers";
import { expect } from "vitest";
// scheduled controllers cannot cross the loopback binding, so the suite calls the shim directly
// @ts-expect-error worker-build emits no declaration for its JavaScript shim
import RustWorker from "../build/index.js";

export const BASE = "https://machines.test";
export const CLIENT = "client-homebased";
export const OTHER_CLIENT = "client-other";
export const MINUTE = 60_000;
export const HOUR = 60 * MINUTE;

export interface ReleaseView {
  reason: string;
  by: string;
  at: string;
}

export interface LeaseView {
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

export interface RunView {
  run_id: string;
  name: string;
  class: string;
  max_machines: number;
  deadline_at: string;
  state: "open" | "releasing" | "released";
  created_at: string;
  machines: LeaseView[];
}

export interface ErrorBody {
  error: { code: string; message: string; retryable: boolean };
}

export interface Permit {
  permitId: string;
  ownerId: string;
  generation: number;
  units: number;
}

export async function call(
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

export async function expectError(response: Response, status: number, code: string): Promise<void> {
  expect(response.status).toBe(status);
  const body = (await response.json()) as ErrorBody;
  expect(body.error.code).toBe(code);
}

export function runBody(overrides: Record<string, unknown> = {}) {
  return {
    request_id: crypto.randomUUID(),
    name: "pool-test",
    class: "small",
    max_machines: 4,
    deadline_at: new Date(Date.now() + 2 * HOUR).toISOString(),
    ...overrides,
  };
}

export async function createRun(overrides: Record<string, unknown> = {}): Promise<RunView> {
  const response = await call("POST", "/v1/runs", CLIENT, runBody(overrides));
  expect(response.status).toBe(201);
  return response.json();
}

export function machineBody(hostname: string) {
  return {
    request_id: crypto.randomUUID(),
    hostname,
    boot: {
      files: [{ path: "/etc/homebased/agent.toml", mode: "0600", content: "token = 'x'\n" }],
      run: [["systemctl", "start", "homebased-agent"]],
    },
  };
}

export async function createMachine(run: RunView, hostname: string): Promise<LeaseView> {
  const response = await call("POST", `/v1/runs/${run.run_id}/machines`, CLIENT, machineBody(hostname));
  expect(response.status).toBe(201);
  return response.json();
}

export async function readLease(leaseId: string, token = CLIENT): Promise<LeaseView> {
  const response = await call("GET", `/v1/leases/${leaseId}`, token);
  expect(response.status).toBe(200);
  return response.json();
}

export async function readRun(runId: string): Promise<RunView> {
  const response = await call("GET", `/v1/runs/${runId}`, CLIENT);
  expect(response.status).toBe(200);
  return response.json();
}

export function runStub(runId: string): DurableObjectStub {
  return env.RUNS.get(env.RUNS.idFromName(runId));
}

export function gateStub(): DurableObjectStub {
  return env.GATE.get(env.GATE.idFromName("global"));
}

export async function permits(): Promise<Permit[]> {
  const response = await gateStub().fetch("https://gate/cloudlease/capacity/snapshot");
  expect(response.status).toBe(200);
  return ((await response.json()) as { permits: Permit[] }).permits;
}

export function generationOf(leaseId: string): number {
  return Number(leaseId.split("-").at(-1));
}

// moves a stored time into the past, as if the run object had slept through it
export async function editHolding(
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

export async function backdateMachine(lease: LeaseView, by: number): Promise<void> {
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
export async function settle(lease: LeaseView): Promise<LeaseView> {
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

export async function scheduled(): Promise<void> {
  const controller = createScheduledController({ cron: "*/5 * * * *", scheduledTime: new Date() });
  const context = createExecutionContext();
  const worker = new RustWorker(context, env);

  await worker.scheduled(controller);
  await waitOnExecutionContext(context);
}

