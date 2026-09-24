import type { Page } from "@playwright/test";

export async function mockHost(
  page: Page,
  options: {
    busy?: boolean;
    libraryError?: boolean;
    paired?: boolean;
    rotateError?: boolean;
    /** A live session: busy host, a peer, and filled stats. */
    streaming?: boolean;
    /** Every API call fails, as when the host is not running. */
    offline?: boolean;
  } = {},
) {
  const calls: string[] = [];
  const status = {
    pc_name: "GAMING-PC",
    version: "0.1.1",
    build_id: "dev",
    busy: options.busy ?? options.streaming ?? false,
    available: true,
    https: true,
    tls_mode: "local-ca",
    play_url: "https://gaming-pc.local/",
    ca_url: "/ca.crt",
    active_stream: null,
    remote_mapping: null,
  };
  const games = [
    "Hades II",
    "Hollow Knight",
    "Celeste",
    "Portal 2",
    "Stardew Valley",
    "Outer Wilds",
  ].map((name, id) => ({
    id: String(id),
    kind: "game",
    name,
    source: id % 2 ? "epic" : "steam",
  }));
  await page.route("**/api/v1/**", async (route) => {
    if (options.offline) return route.abort("connectionrefused");
    const path = new URL(route.request().url()).pathname.replace(
      "/api/v1/",
      "",
    );
    calls.push(route.request().method() + " " + path);
    let body: unknown = {};
    let code = 200;
    switch (path) {
      case "status":
        body = status;
        break;
      case "session":
        code = options.paired === false ? 401 : 200;
        break;
      case "library":
        code = options.libraryError ? 503 : 200;
        body = { items: games, art_pending: false };
        break;
      case "capabilities":
        body = { audio: { enabled: false } };
        break;
      case "admin/status":
        body = {
          pin: "482916",
          pin_ttl_secs: 240,
          peer: options.streaming
            ? { browser: "Safari on iPhone", ip: "192.168.1.42" }
            : null,
          uptime_secs: 5400,
          stats: options.streaming
            ? {
                host: {
                  width: 2560,
                  height: 1440,
                  codec: "H265",
                  encoder_bitrate_kbps: 80000,
                },
                transport: {},
                client: {
                  presented_fps: 118.6,
                  inbound_bitrate_kbps: 78200,
                  rtt_ms: 3.4,
                },
                wt_active: true,
              }
            : { host: {}, transport: {}, client: {} },
        };
        break;
      case "admin/controllers":
        body = {
          controllers: [
            {
              id: "test-device",
              name: "Living room <TV>",
              revoked: false,
              last_seen_unix: null,
            },
          ],
        };
        break;
      case "health":
        body = { ok: true, symptoms: [] };
        break;
      case "admin/rotate-pin":
        code = options.rotateError ? 503 : 200;
        body = options.rotateError
          ? { error: "Host unavailable. Try again." }
          : { pin: "194826" };
        break;
      case "admin/pair-invite":
        body = {
          id: "invite-1",
          short_code: "ABCD12345",
          needs_approval: true,
          not_after_unix: Math.floor(Date.now() / 1000) + 60,
          qr_svg:
            '<svg xmlns="http://www.w3.org/2000/svg" width="120" height="120"><rect width="120" height="120" fill="white"/></svg>',
        };
        break;
      case "admin/pair-invites":
        body = { invites: [{ id: "invite-1", used: false, approved: false }] };
        break;
      case "pair":
        code = 401;
        body = { error: "Invitation expired" };
        break;
    }
    await route.fulfill({
      status: code,
      contentType: "application/json",
      body: JSON.stringify(body),
    });
  });
  return calls;
}
export async function openPlayer(page: Page) {
  await page.goto("/");
  await page.evaluate(async () => {
    // Mount the remote entry point on the fixture origin without auto-starting media.
    const { renderPlay } = await import("/src/ui/play.ts");
    await renderPlay(document.querySelector<HTMLElement>("#app")!);
  });
}
