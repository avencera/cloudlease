// a fake ntfy topic that records every alert the Worker posts

import { vi } from "vitest";

export const NTFY_ORIGIN = "https://ntfy.test";

export interface Sent {
  title: string;
  priority: string;
  authorization: string | null;
  body: string;
}

export class FakeNtfy {
  readonly sent: Sent[] = [];

  titles(): string[] {
    return this.sent.map((alert) => alert.title);
  }

  // wraps whatever fetch is installed, so install it after the fake DigitalOcean
  install(): void {
    const inner = globalThis.fetch;

    vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
      const request = input instanceof Request ? input : new Request(input, init);
      if (new URL(request.url).origin !== NTFY_ORIGIN) {
        return inner(input, init);
      }

      this.sent.push({
        title: request.headers.get("title") ?? "",
        priority: request.headers.get("priority") ?? "",
        authorization: request.headers.get("authorization"),
        body: await request.text(),
      });
      return Response.json({ id: "fake" });
    });
  }
}
