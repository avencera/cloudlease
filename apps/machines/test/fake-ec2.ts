// a stateful fake of the EC2 Query actions cloudlease uses, installed over the isolate's
// global fetch so the Rust Worker and its Durable Objects call it

import { vi } from "vitest";

export const EC2_ORIGIN = "https://ec2.aws.test";
const ACCESS_KEY_ID = "test-aws-key";

export interface Tag {
  key: string;
  value: string;
}

export interface Instance {
  id: string;
  state: "pending" | "running" | "shutting-down" | "terminated";
  instanceType: string;
  zone: string;
  subnetId: string;
  imageId: string;
  // the security group comes from the launch template, which a fleet cannot override
  launchTemplate: string;
  userData: string;
  rootDevice: string;
  rootVolumeGib: number;
  rootEncrypted: boolean;
  httpTokens: string;
  spotType: string;
  interruptionBehavior: string;
  allocationStrategy: string | null;
  maxPrice: string | null;
  spotRequestId: string | null;
  // why EC2 last changed the state, such as Server.SpotInstanceTermination
  stateReason: string | null;
  terminatedAt: number | null;
  tags: Tag[];
}

// one launch template override of a CreateFleet call
export interface Pool {
  instanceType: string;
  zone: string;
  subnetId: string;
}

export interface SpotRequest {
  id: string;
  instanceId: string;
  state: "active" | "closed" | "cancelled";
  type: string;
  interruptionBehavior: string;
  // the status code of EC2's latest evaluation, such as marked-for-termination
  statusCode: string;
  tags: Tag[];
}

export class FakeEc2 {
  readonly instances = new Map<string, Instance>();
  readonly spotRequests = new Map<string, SpotRequest>();
  // every action, as `Action detail`
  readonly log: string[] = [];
  // instance types, zones or `type@zone` pairs with no Spot capacity
  readonly noCapacity = new Set<string>();
  // the pools of each CreateFleet call, as `type@zone`, in the order sent
  readonly fleets: string[][] = [];
  // fails every instance list, as during an EC2 outage
  failLists = false;
  // awaited before the next call is answered, then cleared
  beforeAnswer: ((action: string) => Promise<void>) | undefined;
  // client token to the response the first call got, like EC2's idempotent CreateFleet
  private readonly tokens = new Map<string, string>();
  private nextId = 1;

  addInstance(tags: Tag[], state: Instance["state"] = "running", pool = { instanceType: "c8a.4xlarge", zone: "us-east-1a" }): string {
    const id = this.newId("i");
    const spotRequestId = this.newId("sir");
    this.instances.set(id, {
      id,
      state,
      instanceType: pool.instanceType,
      zone: pool.zone,
      subnetId: "subnet-0123456789abcdef0",
      imageId: "ami-0123456789abcdef0",
      launchTemplate: "lt-0123456789abcdef0:1",
      userData: "",
      rootDevice: "/dev/sda1",
      rootVolumeGib: 8,
      rootEncrypted: true,
      httpTokens: "required",
      spotType: "one-time",
      interruptionBehavior: "terminate",
      allocationStrategy: null,
      maxPrice: null,
      spotRequestId,
      stateReason: null,
      terminatedAt: null,
      tags,
    });
    this.spotRequests.set(spotRequestId, {
      id: spotRequestId,
      instanceId: id,
      state: "active",
      type: "one-time",
      interruptionBehavior: "terminate",
      statusCode: "fulfilled",
      tags,
    });
    return id;
  }

  // gives an instance EC2's two-minute interruption notice: its Spot request is marked for
  // termination and the instance keeps running until EC2 reclaims it
  markForTermination(id: string): void {
    const request = this.spotRequests.get(this.instances.get(id)?.spotRequestId ?? "");
    if (request === undefined) {
      throw new Error(`no Spot request for instance ${id}`);
    }
    request.statusCode = "marked-for-termination";
  }

  // reclaims an instance as EC2 does when it needs the Spot capacity back: a one-time
  // request that terminates on interruption closes, and its instance is terminated and
  // stays listed with its type, zone and state reason
  interrupt(id: string, at = Date.now()): void {
    const instance = this.instances.get(id);
    if (instance === undefined) {
      throw new Error(`no instance ${id}`);
    }
    instance.state = "terminated";
    instance.stateReason = "Server.SpotInstanceTermination";
    instance.terminatedAt = at;
    const request = instance.spotRequestId === null ? undefined : this.spotRequests.get(instance.spotRequestId);
    if (request !== undefined) {
      request.state = "closed";
      request.statusCode = "instance-terminated-no-capacity";
    }
  }

  named(name: string): Instance | undefined {
    return [...this.instances.values()].find((instance) => instance.tags.some((tag) => tag.key === "Name" && tag.value === name));
  }

  launches(): string[] {
    return this.log.filter((entry) => entry.startsWith("CreateFleet "));
  }

  terminationsOf(...ids: string[]): string[] {
    return this.log.filter((entry) => entry.startsWith("TerminateInstances ") && ids.some((id) => entry.endsWith(id)));
  }

  install(): void {
    const originalFetch = globalThis.fetch;

    vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
      const request = input instanceof Request ? input : new Request(input, init);
      if (new URL(request.url).origin !== EC2_ORIGIN) {
        return originalFetch(input, init);
      }
      // the Worker signs with SigV4 under its own key for the region it calls
      const authorization = request.headers.get("authorization") ?? "";
      if (!authorization.startsWith(`AWS4-HMAC-SHA256 Credential=${ACCESS_KEY_ID}/`) || !authorization.includes("/ec2/aws4_request")) {
        return error(401, "AuthFailure", "AWS was not able to validate the provided access credentials");
      }

      const params = new URLSearchParams(new TextDecoder().decode(await request.arrayBuffer()));
      const pause = this.beforeAnswer;
      if (pause !== undefined) {
        // runs once, while the Worker waits on this call, so a test can act in between
        this.beforeAnswer = undefined;
        await pause(params.get("Action") ?? "");
      }
      return this.handle(params);
    });
  }

  private newId(prefix: string): string {
    return `${prefix}-${(this.nextId++).toString(16).padStart(17, "0")}`;
  }

  private handle(params: URLSearchParams): Response {
    const action = params.get("Action") ?? "";
    switch (action) {
      case "CreateFleet":
        return this.createFleet(params);
      case "DescribeInstances":
        return this.describeInstances(params);
      case "TerminateInstances": {
        const id = params.get("InstanceId.1") ?? "";
        this.log.push(`TerminateInstances ${id}`);
        const instance = this.instances.get(id);
        if (instance === undefined) {
          return error(400, "InvalidInstanceID.NotFound", `The instance ID '${id}' does not exist`);
        }
        if (instance.state !== "terminated") {
          instance.state = "terminated";
          instance.stateReason = "Client.UserInitiatedShutdown";
          instance.terminatedAt = Date.now();
        }
        const request = instance.spotRequestId === null ? undefined : this.spotRequests.get(instance.spotRequestId);
        if (request !== undefined && request.type === "one-time") {
          request.state = "closed";
        }
        return xml("TerminateInstancesResponse", "");
      }
      case "DescribeSpotInstanceRequests":
        return this.describeSpotRequests(params);
      case "CancelSpotInstanceRequests": {
        const id = params.get("SpotInstanceRequestId.1") ?? "";
        this.log.push(`CancelSpotInstanceRequests ${id}`);
        const request = this.spotRequests.get(id);
        // the Worker's policy lets it cancel only a request with its managed tags
        if (request !== undefined && !request.tags.some((tag) => tag.key.endsWith("-managed"))) {
          return error(403, "UnauthorizedOperation", "You are not authorized to perform this operation.");
        }
        if (request !== undefined) {
          request.state = "cancelled";
        }
        return xml("CancelSpotInstanceRequestsResponse", "");
      }
      default:
        return error(400, "InvalidAction", `unexpected action ${action}`);
    }
  }

  private refuses(pool: Pool): boolean {
    return this.noCapacity.has(pool.instanceType) || this.noCapacity.has(pool.zone) || this.noCapacity.has(`${pool.instanceType}@${pool.zone}`);
  }

  // an instant fleet of one Spot instance: it launches in the first pool, by the order sent,
  // that has capacity, where EC2 would weigh price and capacity, and lists every pool it
  // passed over in its error set
  private createFleet(params: URLSearchParams): Response {
    const pools = overridesOf(params);
    this.log.push(`CreateFleet ${pools.map((pool) => `${pool.instanceType}@${pool.zone}`).join(",")}`);
    this.fleets.push(pools.map((pool) => `${pool.instanceType}@${pool.zone}`));
    const invalid = invalidFleet(params, pools);
    if (invalid !== undefined) {
      return error(400, "InvalidParameterValue", invalid);
    }
    const token = params.get("ClientToken") ?? "";
    const replayed = this.tokens.get(token);
    if (replayed !== undefined) {
      return fleetResponse(replayed);
    }

    const template = "LaunchTemplateConfigs.1.LaunchTemplateSpecification";
    const errors: string[] = [];
    let launched: string | undefined;
    for (const [index, pool] of pools.entries()) {
      if (this.refuses(pool)) {
        errors.push(fleetError("InsufficientInstanceCapacity", "There is no Spot capacity available that matches your request.", pool));
        continue;
      }

      const override = `LaunchTemplateConfigs.1.Overrides.${index + 1}`;
      const device = `${override}.BlockDeviceMapping.1`;
      const id = this.newId("i");
      const spotRequestId = this.newId("sir");
      const instance: Instance = {
        id,
        state: "pending",
        instanceType: pool.instanceType,
        zone: pool.zone,
        subnetId: pool.subnetId,
        imageId: params.get(`${override}.ImageId`) ?? "",
        launchTemplate: `${params.get(`${template}.LaunchTemplateId`)}:${params.get(`${template}.Version`)}`,
        userData: atob(params.get(`${template}.LaunchTemplateSpecificationUserData`) ?? ""),
        rootDevice: params.get(`${device}.DeviceName`) ?? "",
        rootVolumeGib: Number(params.get(`${device}.Ebs.VolumeSize`)),
        rootEncrypted: params.get(`${device}.Ebs.Encrypted`) === "true",
        httpTokens: params.get(`${override}.MetadataOptions.HttpTokens`) ?? "optional",
        // an instant fleet's Spot request is always one-time
        spotType: "one-time",
        interruptionBehavior: params.get("SpotOptions.InstanceInterruptionBehavior") ?? "",
        allocationStrategy: params.get("SpotOptions.AllocationStrategy"),
        maxPrice: params.get(`${override}.MaxPrice`),
        spotRequestId,
        stateReason: null,
        terminatedAt: null,
        tags: taggedAs(params, "instance"),
      };
      this.instances.set(id, instance);
      // a fleet cannot tag its Spot request
      this.spotRequests.set(spotRequestId, {
        id: spotRequestId,
        instanceId: id,
        state: "active",
        type: "one-time",
        interruptionBehavior: instance.interruptionBehavior,
        statusCode: "fulfilled",
        tags: [],
      });
      // an instant fleet reports the instance's zone and subnet beside the override too
      launched =
        `<item><instanceIds><item>${id}</item></instanceIds><instanceType>${pool.instanceType}</instanceType>` +
        `<availabilityZone>${pool.zone}</availabilityZone><subnetId>${pool.subnetId}</subnetId>` +
        `${launchTemplateAndOverrides(pool)}<lifecycle>spot</lifecycle></item>`;
      break;
    }

    const body =
      `<fleetId>${this.newId("fleet")}</fleetId><errorSet>${errors.join("")}</errorSet>` +
      `<fleetInstanceSet>${launched ?? ""}</fleetInstanceSet>`;
    this.tokens.set(token, body);
    return fleetResponse(body);
  }

  private describeInstances(params: URLSearchParams): Response {
    const id = params.get("InstanceId.1");
    this.log.push(id === null ? "DescribeInstances" : `DescribeInstances ${id}`);
    if (id !== null) {
      const instance = this.instances.get(id);
      if (instance === undefined) {
        return error(400, "InvalidInstanceID.NotFound", `The instance ID '${id}' does not exist`);
      }
      return describeResponse([instance]);
    }
    if (this.failLists) {
      return error(503, "Unavailable", "The service is unavailable");
    }

    const filter = params.get("Filter.1.Name") ?? "";
    const value = params.get("Filter.1.Value.1") ?? "";
    const matches = (instance: Instance) => {
      if (filter === "tag-key") {
        return instance.tags.some((tag) => tag.key === value);
      }
      const key = filter.replace(/^tag:/, "");
      return instance.tags.some((tag) => tag.key === key && tag.value === value);
    };
    return describeResponse([...this.instances.values()].filter(matches));
  }

  // EC2 refuses a call that names a request that does not exist, and leaves out the named
  // requests a filter does not match
  private describeSpotRequests(params: URLSearchParams): Response {
    const ids = indexed(params, "SpotInstanceRequestId");
    this.log.push(ids.length === 0 ? "DescribeSpotInstanceRequests" : `DescribeSpotInstanceRequests ${ids.join(",")}`);
    const missing = ids.find((id) => !this.spotRequests.has(id));
    if (missing !== undefined) {
      return error(400, "InvalidSpotInstanceRequestID.NotFound", `The spot instance request ID '${missing}' does not exist`);
    }
    const fields: Record<string, (request: SpotRequest) => string> = {
      "instance-id": (request) => request.instanceId,
      "status-code": (request) => request.statusCode,
    };
    const filters: [(request: SpotRequest) => string, string[]][] = [];
    for (let index = 1; params.has(`Filter.${index}.Name`); index += 1) {
      const name = params.get(`Filter.${index}.Name`) ?? "";
      const field = fields[name];
      if (field === undefined) {
        return error(400, "InvalidParameterValue", `unexpected filter ${name}`);
      }
      filters.push([field, indexed(params, `Filter.${index}.Value`)]);
    }
    const requests = [...this.spotRequests.values()].filter(
      (request) => (ids.length === 0 || ids.includes(request.id)) && filters.every(([field, values]) => values.includes(field(request))),
    );
    const items = requests
      .map(
        (request) =>
          `<item><spotInstanceRequestId>${request.id}</spotInstanceRequestId><instanceId>${request.instanceId}</instanceId>` +
          `<state>${request.state}</state><type>${request.type}</type><status><code>${request.statusCode}</code></status>` +
          `<instanceInterruptionBehavior>${request.interruptionBehavior}</instanceInterruptionBehavior>${tagSet(request.tags)}</item>`,
      )
      .join("");
    return xml("DescribeSpotInstanceRequestsResponse", `<spotInstanceRequestSet>${items}</spotInstanceRequestSet>`);
  }
}

// the values of a query list such as SpotInstanceRequestId.1, SpotInstanceRequestId.2, …
function indexed(params: URLSearchParams, prefix: string): string[] {
  const values: string[] = [];
  for (let index = 1; params.has(`${prefix}.${index}`); index += 1) {
    values.push(params.get(`${prefix}.${index}`) ?? "");
  }
  return values;
}

function overridesOf(params: URLSearchParams): Pool[] {
  const pools: Pool[] = [];
  for (let index = 1; params.has(`LaunchTemplateConfigs.1.Overrides.${index}.InstanceType`); index += 1) {
    const prefix = `LaunchTemplateConfigs.1.Overrides.${index}`;
    pools.push({
      instanceType: params.get(`${prefix}.InstanceType`) ?? "",
      zone: params.get(`${prefix}.AvailabilityZone`) ?? "",
      subnetId: params.get(`${prefix}.SubnetId`) ?? "",
    });
  }
  return pools;
}

// what EC2 refuses outright, and what cloudlease must always send
function invalidFleet(params: URLSearchParams, pools: Pool[]): string | undefined {
  const template = "LaunchTemplateConfigs.1.LaunchTemplateSpecification";
  const expected: Record<string, string> = {
    Type: "instant",
    "TargetCapacitySpecification.TotalTargetCapacity": "1",
    "TargetCapacitySpecification.DefaultTargetCapacityType": "spot",
    "SpotOptions.AllocationStrategy": "price-capacity-optimized",
    "SpotOptions.InstanceInterruptionBehavior": "terminate",
  };
  for (const [name, value] of Object.entries(expected)) {
    if (params.get(name) !== value) {
      return `${name} must be ${value}`;
    }
  }
  if (!/^[1-9][0-9]*$/.test(params.get(`${template}.Version`) ?? "")) {
    return "the launch template version must be pinned to a number";
  }
  if (!/^lt-[0-9a-f]{17}$/.test(params.get(`${template}.LaunchTemplateId`) ?? "")) {
    return "the launch template ID is invalid";
  }
  if (pools.length === 0 || pools.some((pool) => pool.subnetId === "" || pool.zone === "")) {
    return "every override needs a subnet and a zone";
  }
  if ([...params.keys()].some((name) => name.includes("SecurityGroup") || name.includes("NetworkInterface"))) {
    return "a fleet cannot override security groups or network interfaces";
  }
  return undefined;
}

// the tags of one TagSpecification resource type
function taggedAs(params: URLSearchParams, resourceType: string): Tag[] {
  for (let index = 1; params.has(`TagSpecification.${index}.ResourceType`); index += 1) {
    if (params.get(`TagSpecification.${index}.ResourceType`) === resourceType) {
      return tagsOf(params, `TagSpecification.${index}`);
    }
  }
  return [];
}

function launchTemplateAndOverrides(pool: Pool): string {
  return (
    "<launchTemplateAndOverrides><launchTemplateSpecification><version>1</version></launchTemplateSpecification>" +
    `<overrides><instanceType>${pool.instanceType}</instanceType><availabilityZone>${pool.zone}</availabilityZone>` +
    `<subnetId>${pool.subnetId}</subnetId></overrides></launchTemplateAndOverrides>`
  );
}

function fleetError(code: string, message: string, pool: Pool): string {
  return (
    `<item><errorCode>${code}</errorCode><errorMessage>${escape(message)}</errorMessage>` +
    `${launchTemplateAndOverrides(pool)}<lifecycle>spot</lifecycle></item>`
  );
}

// EC2 answers a fleet that launched nothing with HTTP 200 and an empty instance set
function fleetResponse(body: string): Response {
  return xml("CreateFleetResponse", body);
}

function tagsOf(params: URLSearchParams, prefix: string): Tag[] {
  const tags: Tag[] = [];
  for (let index = 1; params.has(`${prefix}.Tag.${index}.Key`); index += 1) {
    tags.push({
      key: params.get(`${prefix}.Tag.${index}.Key`) ?? "",
      value: params.get(`${prefix}.Tag.${index}.Value`) ?? "",
    });
  }
  return tags;
}

function escape(value: string): string {
  return value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
}

function tagSet(tags: Tag[]): string {
  const items = tags.map((tag) => `<item><key>${escape(tag.key)}</key><value>${escape(tag.value)}</value></item>`).join("");
  return `<tagSet>${items}</tagSet>`;
}

function xml(root: string, body: string): Response {
  return new Response(`<${root} xmlns="http://ec2.amazonaws.com/doc/2016-11-15/"><requestId>fake</requestId>${body}</${root}>`, {
    headers: { "content-type": "text/xml" },
  });
}

function error(status: number, code: string, message: string): Response {
  return new Response(
    `<Response><Errors><Error><Code>${code}</Code><Message>${escape(message)}</Message></Error></Errors><RequestID>fake</RequestID></Response>`,
    { status, headers: { "content-type": "text/xml" } },
  );
}

// the form EC2 documents for a user-initiated change's transition reason
function transitionReason(at: number): string {
  const time = new Date(at).toISOString().replace("T", " ").replace(/\.\d+Z$/, " GMT");
  return `<reason>Service initiated (${time})</reason>`;
}

// a retiring instance keeps its type and zone but no network fields, as EC2 describes it
function instanceXml(instance: Instance): string {
  const state = `<instanceState><name>${instance.state}</name></instanceState>`;
  const spot = instance.spotRequestId === null ? "" : `<spotInstanceRequestId>${instance.spotRequestId}</spotInstanceRequestId>`;
  const placement =
    `<instanceType>${instance.instanceType}</instanceType>` + `<placement><availabilityZone>${instance.zone}</availabilityZone></placement>`;
  if (instance.state === "shutting-down" || instance.state === "terminated") {
    const reason =
      instance.stateReason === null
        ? ""
        : `${instance.terminatedAt === null ? "" : transitionReason(instance.terminatedAt)}` +
          `<stateReason><code>${instance.stateReason}</code><message>${instance.stateReason}</message></stateReason>`;
    return `<item><instanceId>${instance.id}</instanceId>${state}${placement}${reason}${spot}${tagSet(instance.tags)}</item>`;
  }
  return (
    `<item><instanceId>${instance.id}</instanceId><imageId>${instance.imageId}</imageId>${state}${placement}` +
    `<subnetId>${instance.subnetId}</subnetId><groupSet><item><groupId>sg-0123456789abcdef0</groupId></item></groupSet>` +
    `<instanceLifecycle>spot</instanceLifecycle>${spot}<metadataOptions><httpTokens>${instance.httpTokens}</httpTokens></metadataOptions>` +
    `${tagSet(instance.tags)}</item>`
  );
}

function describeResponse(instances: Instance[]): Response {
  const items = instances.map(instanceXml).join("");
  return xml("DescribeInstancesResponse", `<reservationSet><item><instancesSet>${items}</instancesSet></item></reservationSet>`);
}

// the tags cloudlease writes on a machine's instance, its root volume and its fleet
export function instanceTags(owner: string, generation: number, untilSeconds?: number, prefix = "clm"): Tag[] {
  const tags: Tag[] = [
    { key: `${prefix}-managed`, value: "true" },
    { key: `${prefix}-owner`, value: owner },
    { key: `${prefix}-gen`, value: String(generation) },
    { key: `${prefix}-kind`, value: "instance" },
  ];
  if (untilSeconds !== undefined) {
    tags.push({ key: `${prefix}-until`, value: String(untilSeconds) });
  }
  return tags;
}
