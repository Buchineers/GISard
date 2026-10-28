import { readFileSync, writeFileSync } from "node:fs";
import { performance } from "node:perf_hooks";
import { resolve } from "node:path";

import { expect, test } from "vitest";

import { parseBuildingDatasetText } from "../src/core/dataset-loader.js";
import { evaluateValidatedSubproblemAsync } from "../src/core/evaluation-engine.js";
import { parseSolutionText } from "../src/core/solution-parser.js";
import { validateSubproblemInput } from "../src/core/submission-validator.js";

interface Summary {
  index: number;
  tau: number | undefined;
  k: number | undefined;
  claims: number;
  verified: number;
  invalidAntennas: number;
  snappedAntennas: number;
  nonSnapWarnings: string[];
  seconds: number;
}

test("officially evaluates the final GISard submission", async () => {
  const datasetPath = requiredEnvironment("GISARD_OFFICIAL_DATASET");
  const solutionPath = requiredEnvironment("GISARD_OFFICIAL_SOLUTION");
  const reportPath = requiredEnvironment("GISARD_OFFICIAL_REPORT");
  const dataset = parseBuildingDatasetText(readFileSync(resolve(datasetPath), "utf8"));
  const solution = parseSolutionText(readFileSync(resolve(solutionPath), "utf8"));
  const summaries: Summary[] = [];

  expect(solution.subproblems).toHaveLength(9);
  for (const source of solution.subproblems) {
    const expectedK = source.k;
    if (expectedK === undefined) throw new Error(`Subproblem ${source.index} has no valid k.`);
    const validated = validateSubproblemInput(source, dataset);
    const started = performance.now();
    const result = await evaluateValidatedSubproblemAsync(dataset, validated, {
      onAntennaProgress: (progress) => {
        if (
          progress.completedAntennaCount === progress.totalAntennaCount
          || progress.completedAntennaCount % 25 === 0
        ) {
          console.log(
            `OFFICIAL_PROGRESS ${source.index} ${progress.completedAntennaCount}/${progress.totalAntennaCount} `
            + `${progress.verifiedClaimCount}/${progress.totalClaimCount}`,
          );
        }
      },
    });
    const warnings = result.warnings
      .filter((warning) => warning.code !== "ANTENNA_SNAPPED")
      .map((warning) => `${warning.code}: ${warning.message}`);
    const summary: Summary = {
      index: source.index,
      tau: source.tau,
      k: source.k,
      claims: validated.claims.uniqueKnownIds.length,
      verified: result.verifiedServiceScore,
      invalidAntennas: validated.invalidRetainedAntennaCount,
      snappedAntennas: validated.validAntennas.filter((antenna) => antenna.snapped).length,
      nonSnapWarnings: warnings,
      seconds: Math.round((performance.now() - started) / 10) / 100,
    };
    summaries.push(summary);
    writeFileSync(resolve(reportPath), `${JSON.stringify(summaries, null, 2)}\n`);
    console.log(`OFFICIAL_RESULT ${JSON.stringify(summary)}`);

    expect(validated.validAntennas, `subproblem ${source.index} valid antennas`).toHaveLength(expectedK);
    expect(summary.invalidAntennas, `subproblem ${source.index} invalid antennas`).toBe(0);
    expect(summary.nonSnapWarnings, `subproblem ${source.index} warnings`).toEqual([]);
    expect(summary.verified, `subproblem ${source.index} verified claims`).toBe(summary.claims);
  }
});

function requiredEnvironment(name: string): string {
  const value = process.env[name];
  if (value === undefined || value.length === 0) throw new Error(`Missing ${name}.`);
  return value;
}
