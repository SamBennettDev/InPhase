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
