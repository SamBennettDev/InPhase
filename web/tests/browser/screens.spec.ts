// Screenshots of every screen and state, desktop and phone, for design review
// and the README images. Asserts only that each screen rendered without page
// errors; the images land in test-results/.
import { test, expect, type Page } from "@playwright/test";
import { mockHost, openPlayer } from "./fixtures.js";

const VIEWPORTS = {
  desktop: { width: 1440, height: 900 },
  phone: { width: 390, height: 844 },
};

async function shoot(page: Page, name: string, info: { outputPath: (p: string) => string }) {
  await page.waitForTimeout(400);
  await page.screenshot({ path: info.outputPath(`${name}.png`), fullPage: true });
}

for (const [vp, size] of Object.entries(VIEWPORTS)) {
  test.describe(vp, () => {
    test.use({ viewport: size });

    test("dashboard", async ({ page }, info) => {
      const errors: string[] = [];
      page.on("pageerror", (e) => errors.push(e.message));
      await mockHost(page);
      await page.goto("/");
      await expect(page.locator("#pin")).toHaveText("482916");
      await shoot(page, `dashboard-${vp}`, info);
      await page.locator("#new-device").click();
      await expect(page.getByAltText("Scan to pair this device")).toBeVisible();
      await shoot(page, `dashboard-invite-${vp}`, info);
      expect(errors).toEqual([]);
    });

    test("dashboard streaming", async ({ page }, info) => {
      await mockHost(page, { streaming: true });
      await page.goto("/");
      await expect(page.locator("#pin")).toHaveText("482916");
      await shoot(page, `dashboard-streaming-${vp}`, info);
    });

    test("dashboard offline", async ({ page }, info) => {
      await mockHost(page, { offline: true });
      await page.goto("/");
      await page.waitForTimeout(1500);
      await shoot(page, `dashboard-offline-${vp}`, info);
    });

    test("player pairing", async ({ page }, info) => {
      await mockHost(page, { paired: false });
      await openPlayer(page);
      await expect(page.getByLabel("Pairing PIN")).toBeVisible();
      await shoot(page, `player-pin-${vp}`, info);
    });

    test("invite pairing page", async ({ page }, info) => {
      await mockHost(page);
      await page.goto("/pair");
      await expect(page.getByLabel("Pairing code")).toBeVisible();
      await shoot(page, `pair-page-${vp}`, info);
    });

    test("player offline", async ({ page }, info) => {
      await mockHost(page, { offline: true });
      await openPlayer(page);
      await page.waitForTimeout(1500);
      await shoot(page, `player-offline-${vp}`, info);
    });

    test("library", async ({ page }, info) => {
      const errors: string[] = [];
      page.on("pageerror", (e) => errors.push(e.message));
      await mockHost(page);
      await openPlayer(page);
      await expect(page.locator(".lib-card")).toHaveCount(6);
      await shoot(page, `library-${vp}`, info);
      await page.getByRole("button", { name: "Stream settings", exact: true }).click();
      await expect(page.getByRole("dialog")).toBeVisible();
      await shoot(page, `library-settings-${vp}`, info);
      expect(errors).toEqual([]);
    });

    test("library busy and failed", async ({ page }, info) => {
      await mockHost(page, { busy: true, libraryError: true });
      await openPlayer(page);
      await expect(page.locator("#library")).toContainText("could not be loaded");
      await shoot(page, `library-busy-error-${vp}`, info);
    });

    test("in-stream hud", async ({ page }, info) => {
      const errors: string[] = [];
      page.on("pageerror", (e) => errors.push(e.message));
      await mockHost(page);
      await page.goto("/");
      await page.evaluate(async () => {
        const { Hud } = await import("/src/ui/hud.ts");
        const { loadSettings } = await import("/src/settings.ts");
        document.querySelector("#app")!.innerHTML = "";
        const stage = document.createElement("div");
        stage.className = "stage";
        // Stand-in for the picture: something with detail, so contrast shows.
        stage.style.background =
          "linear-gradient(135deg, #3b5b7a, #1c2a38 45%, #6b4a2f)";
        document.body.append(stage);
        const hud = new Hud(
          { ...loadSettings(), showMetrics: true },
          () => {},
          () => {},
          () => {},
        );
        hud.setHasAudio(true);
        stage.append(hud.root);
        hud.update({
          decodedFps: 119.6,
          presentedFps: 119.2,
          rttMs: 3.1,
          inputRttMs: 4.2,
          e2eEstMs: 24,
          width: 2560,
          height: 1440,
          inboundKbps: 78200,
          codec: "H265",
          decodeMs: 6.4,
          path: "LAN",
          audioState: "signal",
        });
      });
      await shoot(page, `hud-${vp}`, info);
      await page.locator('[data-act="panel"]').click();
      await shoot(page, `hud-panel-${vp}`, info);
      expect(errors).toEqual([]);
    });
  });
}
