// SPDX-License-Identifier: GPL-3.0-or-later
import { NETWORK_PROFILES, simulateNetwork, verifyNetworkResult } from "../tests/performance/network-simulation.js";
import { writeFileSync } from "node:fs";

const seconds = Number(process.env.INPHASE_SIM_SECONDS ?? 60);
if (!Number.isInteger(seconds) || seconds < 10 || seconds > 600) throw new Error("INPHASE_SIM_SECONDS must be 10..600");
const frameBytes = Number(process.env.INPHASE_SIM_FRAME_BYTES ?? 12_113);
if (!Number.isInteger(frameBytes) || frameBytes < 100 || frameBytes > 100_000) throw new Error("INPHASE_SIM_FRAME_BYTES must be 100..100000");
const repairLimiter = process.env.INPHASE_SIM_REPAIR_LIMITER ?? "fractional";
if (repairLimiter !== "legacy" && repairLimiter !== "fractional") throw new Error("INPHASE_SIM_REPAIR_LIMITER must be legacy or fractional");
const results = NETWORK_PROFILES.flatMap((profile) => [60, 120].flatMap((fps) =>
  [1, 7, 42].map((seed) => simulateNetwork(profile, fps, seed, seconds, { frameBytes, repairLimiter }))));
console.table(results.map(({ profile, fps, seed, admittedPercent, ageP95Ms, maxGapMs, maxHeld, nacks, resyncs }) =>
  ({ profile, fps, seed, admittedPercent, ageP95Ms, maxGapMs, maxHeld, nacks, resyncs })));
if (process.argv[2]) writeFileSync(process.argv[2], JSON.stringify({ kind: "virtual-network-receiver", seconds, results }, null, 2) + "\n");
for (const result of results) verifyNetworkResult(result, NETWORK_PROFILES.find((p) => p.name === result.profile)!);
