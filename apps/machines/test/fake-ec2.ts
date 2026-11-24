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
  securityGroupId: string;
  userData: string;
  rootDevice: string;
  rootVolumeGib: number;
  spotType: string;
  interruptionBehavior: string;
  maxPrice: string | null;
  spotRequestId: string | null;
  tags: Tag[];
}

export interface SpotRequest {
  id: string;
  instanceId: string;
  state: "active" | "closed" | "cancelled";
  type: string;
  interruptionBehavior: string;
  tags: Tag[];
}

export class FakeEc2 {
  readonly instances = new Map<string, Instance>();
  readonly spotRequests = new Map<string, SpotRequest>();
  // every action, as `Action detail`
  readonly log: string[] = [];
  // instance types, zones or `type@zone` pairs with no Spot capacity
  readonly noCapacity = new Set<string>();
  // fails every instance list, as during an EC2 outage
  failLists = false;
  private readonly tokens = new Map<string, string>();
  private nextId = 1;

  addInstance(tags: Tag[], state: Instance["state"] = "running"): string {
    const id = this.newId("i");
    const spotRequestId = this.newId("sir");
    this.instances.set(id, {
      id,
      state,
      instanceType: "c8a.4xlarge",
      zone: "us-east-1a",
      subnetId: "subnet-0123456789abcdef0",
      imageId: "ami-0123456789abcdef0",
      securityGroupId: "sg-0123456789abcdef0",
      userData: "",
      rootDevice: "/dev/sda1",
      rootVolumeGib: 8,
      spotType: "one-time",
      interruptionBehavior: "terminate",
      maxPrice: null,
      spotRequestId,
      tags,
    });
    this.spotRequests.set(spotRequestId, {
      id: spotRequestId,
      instanceId: id,
      state: "active",
      type: "one-time",
      interruptionBehavior: "terminate",
      tags,
    });
    return id;
  }

  // reclaims an instance as EC2 does when it needs the Spot capacity back: a one-time
  // request that terminates on interruption closes, and its instance is terminated
  interrupt(id: string): void {
    const instance = this.instances.get(id);
    if (instance === undefined) {
      throw new Error(`no instance ${id}`);
    }
    instance.state = "terminated";
    const request = instance.spotRequestId === null ? undefined : this.spotRequests.get(instance.spotRequestId);
    if (request !== undefined) {
      request.state = "closed";
    }
  }

  named(name: string): Instance | undefined {
    return [...this.instances.values()].find((instance) => instance.tags.some((tag) => tag.key === "Name" && tag.value === name));
  }

  launches(): string[] {
    return this.log.filter((entry) => entry.startsWith("RunInstances "));
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
      return this.handle(params);
    });
  }

  private newId(prefix: string): string {
    return `${prefix}-${(this.nextId++).toString(16).padStart(17, "0")}`;
  }

  private handle(params: URLSearchParams): Response {
    const action = params.get("Action") ?? "";
    switch (action) {
      case "RunInstances":
        return this.runInstances(params);
      case "DescribeInstances":
        return this.describeInstances(params);
      case "TerminateInstances": {
        const id = params.get("InstanceId.1") ?? "";
        this.log.push(`TerminateInstances ${id}`);
        const instance = this.instances.get(id);
        if (instance === undefined) {
          return error(400, "InvalidInstanceID.NotFound", `The instance ID '${id}' does not exist`);
        }
        instance.state = "terminated";
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
        if (request !== undefined) {
          request.state = "cancelled";
        }
        return xml("CancelSpotInstanceRequestsResponse", "");
      }
      default:
        return error(400, "InvalidAction", `unexpected action ${action}`);
    }
  }

  private runInstances(params: URLSearchParams): Response {
    const instanceType = params.get("InstanceType") ?? "";
    const zone = params.get("Placement.AvailabilityZone") ?? "";
    this.log.push(`RunInstances ${instanceType} ${zone}`);
    const token = params.get("ClientToken") ?? "";
    const existing = this.tokens.get(token);
    if (existing !== undefined) {
      const instance = this.instances.get(existing) as Instance;
      return runResponse(instance);
    }
    if (this.noCapacity.has(instanceType) || this.noCapacity.has(zone) || this.noCapacity.has(`${instanceType}@${zone}`)) {
      return error(500, "InsufficientInstanceCapacity", `There is no Spot capacity available that matches your request.`);
    }

    const id = this.newId("i");
    const spotRequestId = this.newId("sir");
    const instance: Instance = {
      id,
      state: "pending",
      instanceType,
      zone,
      subnetId: params.get("NetworkInterface.1.SubnetId") ?? "",
      imageId: params.get("ImageId") ?? "",
      securityGroupId: params.get("NetworkInterface.1.SecurityGroupId.1") ?? "",
      userData: atob(params.get("UserData") ?? ""),
      rootDevice: params.get("BlockDeviceMapping.1.DeviceName") ?? "",
      rootVolumeGib: Number(params.get("BlockDeviceMapping.1.Ebs.VolumeSize")),
      spotType: params.get("InstanceMarketOptions.SpotOptions.SpotInstanceType") ?? "",
      interruptionBehavior: params.get("InstanceMarketOptions.SpotOptions.InstanceInterruptionBehavior") ?? "",
      maxPrice: params.get("InstanceMarketOptions.SpotOptions.MaxPrice"),
      spotRequestId,
      tags: tagsOf(params, "TagSpecification.1"),
    };
    this.instances.set(id, instance);
    this.spotRequests.set(spotRequestId, {
      id: spotRequestId,
      instanceId: id,
      state: "active",
      type: instance.spotType,
      interruptionBehavior: instance.interruptionBehavior,
      tags: tagsOf(params, "TagSpecification.2"),
    });
    this.tokens.set(token, id);
    return runResponse(instance);
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

  private describeSpotRequests(params: URLSearchParams): Response {
    const id = params.get("SpotInstanceRequestId.1");
    const instanceId = params.get("Filter.1.Name") === "instance-id" ? params.get("Filter.1.Value.1") : null;
    const requests = [...this.spotRequests.values()].filter(
      (request) => (id === null || request.id === id) && (instanceId === null || request.instanceId === instanceId),
    );
    if (id !== null && requests.length === 0) {
      return error(400, "InvalidSpotInstanceRequestID.NotFound", `The spot instance request ID '${id}' does not exist`);
    }
    const items = requests
      .map(
        (request) =>
          `<item><spotInstanceRequestId>${request.id}</spotInstanceRequestId><instanceId>${request.instanceId}</instanceId>` +
          `<state>${request.state}</state><type>${request.type}</type>` +
          `<instanceInterruptionBehavior>${request.interruptionBehavior}</instanceInterruptionBehavior>${tagSet(request.tags)}</item>`,
      )
      .join("");
    return xml("DescribeSpotInstanceRequestsResponse", `<spotInstanceRequestSet>${items}</spotInstanceRequestSet>`);
  }
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

function runResponse(instance: Instance): Response {
  return xml(
    "RunInstancesResponse",
    `<instancesSet><item><instanceId>${instance.id}</instanceId><spotInstanceRequestId>${instance.spotRequestId}</spotInstanceRequestId></item></instancesSet>`,
  );
}

// a retiring instance carries no network or placement fields, as EC2 describes it
function instanceXml(instance: Instance): string {
  const state = `<instanceState><name>${instance.state}</name></instanceState>`;
  const spot = instance.spotRequestId === null ? "" : `<spotInstanceRequestId>${instance.spotRequestId}</spotInstanceRequestId>`;
  if (instance.state === "shutting-down" || instance.state === "terminated") {
    return `<item><instanceId>${instance.id}</instanceId>${state}${spot}${tagSet(instance.tags)}</item>`;
  }
  return (
    `<item><instanceId>${instance.id}</instanceId><imageId>${instance.imageId}</imageId>${state}` +
    `<instanceType>${instance.instanceType}</instanceType><placement><availabilityZone>${instance.zone}</availabilityZone></placement>` +
    `<subnetId>${instance.subnetId}</subnetId><groupSet><item><groupId>${instance.securityGroupId}</groupId></item></groupSet>` +
    `<instanceLifecycle>spot</instanceLifecycle>${spot}<metadataOptions><httpTokens>required</httpTokens></metadataOptions>` +
    `${tagSet(instance.tags)}</item>`
  );
}

function describeResponse(instances: Instance[]): Response {
  const items = instances.map(instanceXml).join("");
  return xml("DescribeInstancesResponse", `<reservationSet><item><instancesSet>${items}</instancesSet></item></reservationSet>`);
}

// the tags cloudlease writes on a machine's instance and its Spot request
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
