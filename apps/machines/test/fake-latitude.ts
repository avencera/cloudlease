// a stateful fake of the Latitude.sh servers and user data endpoints cloudlease uses,
// installed over the isolate's global fetch so the Rust Worker and its Durable Objects call it

import { vi } from "vitest";

export const LATITUDE_API_ORIGIN = "https://api.latitude.test";
export const LATITUDE_PROJECT = "proj_test";
const KEY = "test-latitude-key";

export interface Server {
  id: string;
  hostname: string;
  project: string;
  site: string;
  plan: string;
  operatingSystem: string;
  userData: string;
  billing: string;
  createdAt: string;
  // a server Latitude.sh has created but does not list or read yet
  hidden: boolean;
}

export interface UserData {
  id: string;
  project: string;
  description: string;
  content: string;
}

export class FakeLatitude {
  readonly servers = new Map<string, Server>();
  readonly userData = new Map<string, UserData>();
  readonly log: string[] = [];
  // sites with no stock of any plan, which answer a create with SERVERS_OUT_OF_STOCK
  readonly outOfStock = new Set<string>();
  // creates the server but answers HTTP 503 and keeps it unlisted, as a create whose
  // response was lost and that Latitude.sh lists late
  loseCreates = false;
  private nextId = 1;

  addServer(hostname: string, createdAt: Date, project = LATITUDE_PROJECT): string {
    const id = this.newId("sv");
    this.servers.set(id, {
      id,
      hostname,
      project,
      site: "DAL",
      plan: "m4-metal-medium",
      operatingSystem: "ubuntu_24_04_x64_lts",
      userData: "",
      billing: "hourly",
      createdAt: createdAt.toISOString(),
      hidden: false,
    });
    return id;
  }

  // deletes of the given resources only, since run objects of earlier tests keep their alarms
  deletesOf(...ids: string[]): string[] {
    return this.log.filter((entry) => entry.startsWith("DELETE ") && ids.some((id) => entry.endsWith(id)));
  }

  serverCreates(): string[] {
    return this.log.filter((entry) => entry.startsWith("POST servers "));
  }

  serverNamed(hostname: string): Server | undefined {
    return [...this.servers.values()].find((server) => server.hostname === hostname);
  }

  install(): void {
    const originalFetch = globalThis.fetch;

    vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
      const request = input instanceof Request ? input : new Request(input, init);
      const url = new URL(request.url);
      if (url.origin !== LATITUDE_API_ORIGIN) {
        return originalFetch(input, init);
      }
      if (request.headers.get("authorization") !== `Bearer ${KEY}`) {
        return error(401, "UNAUTHORIZED", "invalid API key");
      }

      return this.handle(request, url);
    });
  }

  private newId(kind: string): string {
    return `${kind}_${(this.nextId++).toString(36).padStart(13, "0")}`;
  }

  private async handle(request: Request, url: URL): Promise<Response> {
    const method = request.method;
    const [collection = "", id] = url.pathname.replace(/^\//, "").split("/");

    if (method === "POST" && id === undefined) {
      const body = (await request.json()) as { data: { attributes: Record<string, unknown> } };
      return collection === "servers" ? this.createServer(body.data.attributes) : this.createUserData(body.data.attributes);
    }

    this.log.push(id === undefined ? `${method} ${collection}` : `${method} ${collection}/${id}`);
    if (method === "GET" && id === undefined) {
      return this.list(collection, url);
    }

    if (id === undefined) {
      return error(500, "unexpected", `unexpected ${method} ${url.pathname}`);
    }
    const found = collection === "servers" ? this.servers.get(id) : this.userData.get(id);
    if (found === undefined || ("hidden" in found && found.hidden)) {
      return error(404, "not_found", "Specified Record Not Found");
    }
    if (method === "GET") {
      return Response.json({ data: collection === "servers" ? serverJson(found as Server) : userDataJson(found as UserData) });
    }
    if (method === "DELETE") {
      (collection === "servers" ? this.servers : this.userData).delete(id);
      return new Response(null, { status: 204 });
    }

    return error(500, "unexpected", `unexpected ${method} ${url.pathname}`);
  }

  // like Latitude.sh, pages run until an empty one, and an unfiltered list is the whole team
  private list(collection: string, url: URL): Response {
    const project = url.searchParams.get("filter[project]");
    const hostname = url.searchParams.get("filter[hostname][eql]");
    const size = Number(url.searchParams.get("page[size]") ?? "20");
    const number = Number(url.searchParams.get("page[number]") ?? "1");
    const items =
      collection === "servers"
        ? [...this.servers.values()]
            .filter((server) => !server.hidden)
            .filter((server) => project === null || server.project === project)
            .filter((server) => hostname === null || server.hostname === hostname)
            .map(serverJson)
        : [...this.userData.values()]
            .filter((record) => project === null || record.project === project)
            .map(userDataJson);

    return Response.json({ data: items.slice((number - 1) * size, number * size), meta: {} });
  }

  private createServer(attributes: Record<string, unknown>): Response {
    const site = String(attributes.site);
    this.log.push(`POST servers ${site}`);
    if (this.outOfStock.has(site)) {
      return error(422, "SERVERS_OUT_OF_STOCK", "No servers available in stock for the requested plan and site");
    }
    const record = this.userData.get(String(attributes.user_data));
    if (record === undefined) {
      return error(422, "NOT_FOUND", "User data not found");
    }

    const server: Server = {
      id: this.newId("sv"),
      hostname: String(attributes.hostname),
      project: String(attributes.project),
      site,
      plan: String(attributes.plan),
      operatingSystem: String(attributes.operating_system),
      userData: atob(record.content),
      billing: String(attributes.billing),
      createdAt: new Date().toISOString(),
      hidden: this.loseCreates,
    };
    this.servers.set(server.id, server);
    if (this.loseCreates) {
      return error(503, "SERVICE_UNAVAILABLE", "service unavailable");
    }
    return Response.json({ data: serverJson(server) }, { status: 201 });
  }

  private createUserData(attributes: Record<string, unknown>): Response {
    this.log.push("POST user_data");
    const record: UserData = {
      id: this.newId("ud"),
      project: String(attributes.project),
      description: String(attributes.description),
      content: String(attributes.content),
    };
    this.userData.set(record.id, record);
    return Response.json({ data: userDataJson(record) }, { status: 201 });
  }
}

function error(status: number, code: string, detail: string): Response {
  return Response.json({ errors: [{ code, status: String(status), title: "Error", detail, meta: {} }] }, { status });
}

function serverJson(server: Server) {
  return {
    id: server.id,
    type: "servers",
    attributes: {
      hostname: server.hostname,
      status: "on",
      created_at: server.createdAt.replace(/\.\d{3}Z$/, "+00:00"),
      plan: { slug: server.plan, billing: server.billing },
      site: server.site,
    },
  };
}

function userDataJson(record: UserData) {
  return { id: record.id, type: "user_data", attributes: { description: record.description } };
}

// the description cloudlease writes on a machine's user data record
export function recordDescription(owner: string, generation: number, untilSeconds: number, prefix = "clm"): string {
  return [
    `${prefix}-managed`,
    `${prefix}-owner-${owner}`,
    `${prefix}-gen-${generation}`,
    `${prefix}-kind-userdata`,
    `${prefix}-until-${untilSeconds}`,
  ].join(" ");
}
