// SPDX-License-Identifier: GPL-3.0-or-later
import test from "node:test";
import { NETWORK_PROFILES, simulateNetwork, verifyNetworkResult } from "../tests/performance/network-simulation.js";

for (const fps of [60, 120]) {
  for (const profile of NETWORK_PROFILES) {
    test(`${profile.name} at ${fps} fps preserves bytes, decode order and bounded queues`, () => {
      const r = simulateNetwork(profile, fps, 42);
      verifyNetworkResult(r, profile);
    });
  }
}

for (const fps of [60, 120]) {
  test(`a 120ms jitter tail at ${fps} fps does not create a repair flood`, () => {
    const profile = { name: "jitter-tail", rttMs: 40, jitterMs: 120, loss: 0 };
    verifyNetworkResult(simulateNetwork(profile, fps, 42), profile);
  });
}
