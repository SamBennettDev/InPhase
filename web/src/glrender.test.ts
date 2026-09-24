import { test } from "node:test";
import assert from "node:assert/strict";
import { preferWebGl } from "./glrender.js";

const UA = {
  iphone: "Mozilla/5.0 (iPhone; CPU iPhone OS 18_7 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.0 Mobile/15E148 Safari/604.1",
  iphoneChrome: "Mozilla/5.0 (iPhone; CPU iPhone OS 18_7 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/140.0 Mobile/15E148 Safari/604.1",
  macSafari: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.0 Safari/605.1.15",
  chrome: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36",
  edge: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0",
  firefox: "Mozilla/5.0 (X11; Linux x86_64; rv:143.0) Gecko/20100101 Firefox/143.0",
};

test("WebGL for WebKit (every iOS browser, Mac Safari), 2D for Chromium and Firefox", () => {
  assert.equal(preferWebGl(UA.iphone, ""), true);
  assert.equal(preferWebGl(UA.iphoneChrome, ""), true, "iOS Chrome is WebKit");
  assert.equal(preferWebGl(UA.macSafari, ""), true);
  assert.equal(preferWebGl(UA.chrome, ""), false);
  assert.equal(preferWebGl(UA.edge, ""), false);
  assert.equal(preferWebGl(UA.firefox, ""), false);
});

test("?render= overrides the engine default", () => {
  assert.equal(preferWebGl(UA.iphone, "?play&render=2d"), false);
  assert.equal(preferWebGl(UA.chrome, "?play&render=gl"), true);
  assert.equal(preferWebGl(UA.chrome, "?play&render=bogus"), false);
});
