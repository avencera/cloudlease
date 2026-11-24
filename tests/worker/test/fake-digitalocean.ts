// a stateful fake of the DigitalOcean v2 endpoints cloudlease uses, installed over the
// isolate's global fetch so the Rust Worker and its Durable Objects call it

import { vi } from "vitest";

export const PROVIDER_ORIGIN = "https://api.digitalocean.test";

interface Droplet {
  id: number;
  name: string;
  tags: string[];
  // reads that still return the droplet after a delete request, as DigitalOcean deletes
  // droplets asynchronously
  readsUntilGone: number | null;
}

interface Volume {
  id: string;
  name: string;
  tags: string[];
  dropletIds: number[];
  deleted: boolean;
}

export class FakeDigitalOcean {
  readonly droplets = new Map<number, Droplet>();
  readonly volumes = new Map<string, Volume>();
  readonly log: string[] = [];
  private nextId = 9_000;
  private createGate: Promise<void> | null = null;

  addDroplet(id: number, tags: string[], name = `droplet-${id}`): void {
    this.droplets.set(id, { id, name, tags, readsUntilGone: null });
  }

  addVolume(id: string, tags: string[], dropletIds: number[] = []): void {
    this.volumes.set(id, { id, name: `volume-${id}`, tags, dropletIds, deleted: false });
  }

  // owner objects from earlier tests keep their alarms, so a test reads only the deletes of
  // its own resources
  deletesOf(...ids: Array<string | number>): string[] {
    const wanted = new Set(ids.map(String));
    return this.log.filter(
      (entry) => entry.startsWith("DELETE") && wanted.has(entry.split("/").at(-1) ?? ""),
    );
  }

  // holds create responses until the returned function is called; the resource already
  // exists meanwhile, as it does on DigitalOcean while a create response is in flight
  holdCreates(): () => void {
    let release = () => {};
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    this.createGate = gate;
    return () => {
      this.createGate = null;
      release();
    };
  }

  install(): void {
    const originalFetch = globalThis.fetch;

    vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
      const request = input instanceof Request ? input : new Request(input, init);
      const url = new URL(request.url);
      if (url.origin !== PROVIDER_ORIGIN) {
        return originalFetch(input, init);
      }

      return this.handle(request, url);
    });
  }

  private async handle(request: Request, url: URL): Promise<Response> {
    const method = request.method;
    const path = url.pathname.replace(/^\/v2\//, "");
    this.log.push(`${method} ${path}`);
    const [collection, id] = path.split("/");

    if (method === "POST" && collection === "droplets" && id === undefined) {
      const body = (await request.json()) as { name: string; tags: string[] };
      const droplet: Droplet = {
        id: this.nextId++,
        name: body.name,
        tags: body.tags,
        readsUntilGone: null,
      };
      this.droplets.set(droplet.id, droplet);
      await this.createGate;
      return Response.json({ droplet: dropletJson(droplet) }, { status: 202 });
    }

    if (method === "GET" && collection === "droplets" && id === undefined) {
      const tag = url.searchParams.get("tag_name");
      const droplets = [...this.droplets.values()]
        .filter((droplet) => tag === null || droplet.tags.includes(tag))
        .map(dropletJson);
      return Response.json({ droplets, links: {}, meta: { total: droplets.length } });
    }
    if (method === "GET" && collection === "volumes" && id === undefined) {
      const volumes = [...this.volumes.values()].map(volumeJson);
      return Response.json({ volumes, links: {}, meta: { total: volumes.length } });
    }
    if (method === "GET" && collection === "droplets" && id !== undefined) {
      return this.readDroplet(Number(id));
    }
    if (method === "GET" && collection === "volumes" && id !== undefined) {
      const volume = this.volumes.get(id);
      if (volume === undefined || volume.deleted) {
        this.volumes.delete(id);
        return notFound();
      }
      return Response.json({ volume: volumeJson(volume) });
    }
    if (method === "DELETE" && collection === "droplets" && id !== undefined) {
      const droplet = this.droplets.get(Number(id));
      if (droplet === undefined) {
        return notFound();
      }
      droplet.readsUntilGone ??= 1;
      return new Response(null, { status: 204 });
    }
    if (method === "DELETE" && collection === "volumes" && id !== undefined) {
      const volume = this.volumes.get(id);
      if (volume === undefined) {
        return notFound();
      }
      if (volume.dropletIds.length > 0) {
        return Response.json(
          { id: "conflict", message: "the volume is attached to a droplet" },
          { status: 409 },
        );
      }
      volume.deleted = true;
      return new Response(null, { status: 204 });
    }

    return Response.json({ id: "server_error", message: "unexpected call" }, { status: 500 });
  }

  private readDroplet(id: number): Response {
    const droplet = this.droplets.get(id);
    if (droplet === undefined) {
      return notFound();
    }
    if (droplet.readsUntilGone === 0) {
      this.droplets.delete(id);
      for (const volume of this.volumes.values()) {
        volume.dropletIds = volume.dropletIds.filter((attached) => attached !== id);
      }
      return notFound();
    }
    if (droplet.readsUntilGone !== null) {
      droplet.readsUntilGone -= 1;
    }
    return Response.json({ droplet: dropletJson(droplet) });
  }
}

function dropletJson(droplet: Droplet) {
  return { id: droplet.id, name: droplet.name, status: "active", tags: droplet.tags };
}

function volumeJson(volume: Volume) {
  return { id: volume.id, name: volume.name, tags: volume.tags, droplet_ids: volume.dropletIds };
}

function notFound(): Response {
  return Response.json({ id: "not_found", message: "not found" }, { status: 404 });
}

export function tags(
  owner: string,
  generation: number,
  kind: "droplet" | "volume",
  untilSeconds?: number,
): string[] {
  const values = ["cl-managed", `cl-owner-${owner}`, `cl-gen-${generation}`, `cl-kind-${kind}`];
  if (untilSeconds !== undefined) {
    values.push(`cl-until-${untilSeconds}`);
  }
  return values;
}
