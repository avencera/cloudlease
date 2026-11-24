// a stateful fake of the phoenixNAP OAuth, BMC v1 and Tag Manager v1 endpoints cloudlease uses,
// installed over the isolate's global fetch so the Rust Worker and its Durable Objects call it

import { vi } from "vitest";

export const PNAP_API_ORIGIN = "https://api.phoenixnap.test";
export const PNAP_TOKEN_URL = "https://auth.phoenixnap.test/token";

export interface Tag {
  name: string;
  value?: string;
}

export interface Server {
  id: string;
  hostname: string;
  location: string;
  type: string;
  os: string;
  status: string;
  tags: Tag[];
  userData: string;
  // lists that still return the server after its deprovision, as phoenixNAP shows it
  // `deleting` for a while
  listsUntilGone: number | null;
}

export class FakePhoenixNap {
  readonly servers = new Map<string, Server>();
  readonly tagNames = new Set<string>();
  readonly log: string[] = [];
  // locations with no stock of any server type, which answer a create with HTTP 406
  readonly outOfStock = new Set<string>();
  // lists and reads that still return a server after its deprovision
  listsAfterDeprovision = 0;
  // answers every server list with HTTP 503
  failLists = false;
  // answers every server create with HTTP 503, an outcome cloudlease cannot tell from a
  // create that went through
  failCreates = false;
  private nextId = 1;

  addServer(tags: Tag[], hostname = `server-${this.nextId}`): string {
    const id = this.newId();
    this.servers.set(id, {
      id,
      hostname,
      location: "PHX",
      type: "s2.c2.large",
      os: "ubuntu/jammy",
      status: "powered-on",
      tags,
      userData: "",
      listsUntilGone: null,
    });
    return id;
  }

  // deprovisions of the given servers only, since run objects of earlier tests keep their
  // alarms
  deprovisionsOf(...ids: string[]): string[] {
    return this.log.filter(
      (entry) => entry.endsWith("/actions/deprovision") && ids.some((id) => entry.includes(id)),
    );
  }

  creates(): string[] {
    return this.log.filter((entry) => entry.startsWith("POST bmc/v1/servers "));
  }

  serverNamed(hostname: string): Server | undefined {
    return [...this.servers.values()].find((server) => server.hostname === hostname);
  }

  install(): void {
    const originalFetch = globalThis.fetch;

    vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
      const request = input instanceof Request ? input : new Request(input, init);
      if (request.url === PNAP_TOKEN_URL) {
        return Response.json({ access_token: "fake-pnap-token", expires_in: 900, token_type: "Bearer" });
      }
      const url = new URL(request.url);
      if (url.origin !== PNAP_API_ORIGIN) {
        return originalFetch(input, init);
      }
      if (request.headers.get("authorization") !== "Bearer fake-pnap-token") {
        return Response.json({ message: "unauthorized" }, { status: 401 });
      }

      return this.handle(request, url);
    });
  }

  private newId(): string {
    const id = (this.nextId++).toString(16).padStart(24, "0");
    return id;
  }

  private async handle(request: Request, url: URL): Promise<Response> {
    const method = request.method;
    const path = url.pathname.replace(/^\//, "");
    const [, , collection, id, , action] = path.split("/");

    if (path === "tag-manager/v1/tags") {
      return this.tags(request, url);
    }
    if (collection !== "servers") {
      return Response.json({ message: `unexpected ${method} ${path}` }, { status: 500 });
    }

    if (method === "POST" && id === undefined) {
      const body = (await request.json()) as {
        hostname: string;
        location: string;
        type: string;
        os: string;
        tags: Tag[];
        installDefaultSshKeys?: boolean;
        sshKeyIds?: string[];
        osConfiguration: { cloudInit: { userData: string } };
      };
      this.log.push(`POST bmc/v1/servers ${body.location}`);
      // the live API checks this before stock: a Linux server needs at least one SSH key
      if ((body.sshKeyIds ?? []).length === 0 && body.installDefaultSshKeys !== true) {
        return Response.json(
          {
            message: "The request failed due to wrong data.",
            validationErrors: ["At least one SSH key required for operating system"],
          },
          { status: 400 },
        );
      }
      if (this.outOfStock.has(body.location)) {
        return Response.json(
          { message: `Server type ${body.type} is out of stock at ${body.location}` },
          { status: 406 },
        );
      }
      if (this.failCreates) {
        return Response.json({ message: "service unavailable" }, { status: 503 });
      }
      const missing = body.tags.find((tag) => !this.tagNames.has(tag.name));
      if (missing !== undefined) {
        return Response.json({ message: `tag ${missing.name} does not exist` }, { status: 400 });
      }
      const server: Server = {
        id: this.newId(),
        hostname: body.hostname,
        location: body.location,
        type: body.type,
        os: body.os,
        status: "creating",
        tags: body.tags,
        userData: atob(body.osConfiguration.cloudInit.userData),
        listsUntilGone: null,
      };
      this.servers.set(server.id, server);
      return Response.json(serverJson(server), { status: 200 });
    }

    this.log.push(`${method} ${path}`);
    if (method === "GET" && id === undefined && this.failLists) {
      return Response.json({ message: "service unavailable" }, { status: 503 });
    }
    if (method === "GET" && id === undefined) {
      const listed = [...this.servers.values()].filter((server) => this.stillListed(server));
      return Response.json(listed.map(serverJson));
    }
    // phoenixNAP refuses an unknown ID with 403, not 404
    const server = id === undefined ? undefined : this.servers.get(id);
    if (server === undefined || !this.stillListed(server)) {
      return Response.json({ message: "forbidden" }, { status: 403 });
    }
    if (method === "GET") {
      return Response.json(serverJson(server));
    }
    if (method === "POST" && action === "deprovision") {
      server.status = "deleting";
      server.listsUntilGone ??= this.listsAfterDeprovision;
      return Response.json({ result: "Server has been deprovisioned." }, { status: 202 });
    }

    return Response.json({ message: `unexpected ${method} ${path}` }, { status: 500 });
  }

  // a deprovisioned server is shown `deleting` for its remaining lists, then gone
  private stillListed(server: Server): boolean {
    if (server.listsUntilGone === null) {
      return true;
    }
    if (server.listsUntilGone <= 0) {
      this.servers.delete(server.id);
      return false;
    }
    server.listsUntilGone -= 1;
    return true;
  }

  private async tags(request: Request, url: URL): Promise<Response> {
    if (request.method === "GET") {
      this.log.push(`GET tag-manager/v1/tags`);
      const name = url.searchParams.get("name");
      const names = [...this.tagNames].filter((tag) => name === null || tag === name);
      return Response.json(names.map((tag) => ({ id: tag, name: tag })));
    }

    const body = (await request.json()) as { name: string };
    this.log.push(`POST tag-manager/v1/tags ${body.name}`);
    if (this.tagNames.has(body.name)) {
      return Response.json({ message: "tag exists" }, { status: 409 });
    }
    this.tagNames.add(body.name);
    return Response.json({ id: body.name, name: body.name }, { status: 201 });
  }
}

function serverJson(server: Server) {
  return {
    id: server.id,
    hostname: server.hostname,
    status: server.status,
    location: server.location,
    type: server.type,
    os: server.os,
    tags: server.tags.map((tag) => ({ id: tag.name, name: tag.name, value: tag.value ?? null, isBillingTag: false })),
  };
}

// the tag pairs cloudlease writes on a server
export function pnapTags(owner: string, generation: number, untilSeconds?: number, prefix = "clm"): Tag[] {
  const values: Tag[] = [
    { name: `${prefix}-managed` },
    { name: `${prefix}-owner`, value: owner },
    { name: `${prefix}-gen`, value: String(generation) },
    { name: `${prefix}-kind`, value: "server" },
  ];
  if (untilSeconds !== undefined) {
    values.push({ name: `${prefix}-until`, value: String(untilSeconds) });
  }
  return values;
}
