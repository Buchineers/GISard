import { readFileSync, writeFileSync } from "node:fs";

import { test } from "vitest";

import { parseBuildingDatasetText } from "../src/core/dataset-loader.js";
import { evaluateValidatedSubproblemAsync } from "../src/core/evaluation-engine.js";
import { parseSolutionText } from "../src/core/solution-parser.js";
import { validateSubproblemInput } from "../src/core/submission-validator.js";

function required(name: string): string {
  const value = process.env[name];
  if (value === undefined || value === "") throw new Error(`${name} is required`);
  return value;
}

function antennaEntries(path: string): string[] {
  return readFileSync(path, "utf8")
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line !== "")
    .map((line, index) => {
      const match = /^\(?\s*([^,\s()]+)\s*[,\s]\s*([^,\s()]+)\s*\)?$/.exec(line);
      if (match?.[1] === undefined || match[2] === undefined) {
        throw new Error(`malformed antenna line ${index + 1}: ${line}`);
      }
      return `(${match[1]},${match[2]})`;
    });
}

test("scores a GISard placement with the official evaluator", async () => {
  const datasetPath = required("GISARD_OFFICIAL_DATASET");
  const antennaPath = required("GISARD_OFFICIAL_ANTENNAS");
  const outputPath = required("GISARD_OFFICIAL_OUTPUT");
  const tau = Number(required("GISARD_OFFICIAL_TAU"));
  const k = Number(required("GISARD_OFFICIAL_K"));
  if (!(tau > 0 && tau <= 1)) throw new Error(`invalid tau: ${tau}`);
  if (!Number.isSafeInteger(k) || k < 0) throw new Error(`invalid k: ${k}`);

  const dataset = parseBuildingDatasetText(readFileSync(datasetPath, "utf8"));
  const antennas = antennaEntries(antennaPath);
  if (antennas.length !== k) {
    throw new Error(`placement contains ${antennas.length} antennas, expected k=${k}`);
  }

  const allBuildingIds = dataset.buildings.map((building) => building.id);
  const inputText = `(${tau},${k})\n${antennas.join(",")}\n${allBuildingIds.join(",")}`;
  const parsed = parseSolutionText(inputText).subproblems[0];
  if (parsed === undefined) throw new Error("official parser returned no subproblem");
  const validated = validateSubproblemInput(parsed, dataset);
  if (validated.invalidRetainedAntennaCount !== 0) {
    throw new Error(`${validated.invalidRetainedAntennaCount} antennas are invalid`);
  }

  const progressEvery = Math.max(1, Math.ceil(k / 20));
  const started = performance.now();
  const result = await evaluateValidatedSubproblemAsync(dataset, validated, {
    onAntennaProgress: (progress) => {
      if (
        progress.completedAntennaCount % progressEvery === 0
        || progress.completedAntennaCount === progress.totalAntennaCount
      ) {
        console.log(
          `official progress ${progress.completedAntennaCount}/${progress.totalAntennaCount}`
          + ` verified=${progress.verifiedClaimCount}/${progress.totalClaimCount}`,
        );
      }
    },
  });

  const output = `(${tau},${k})\n${antennas.join(",")}\n${result.verifiedBuildingIds.join(",")}\n`;
  writeFileSync(outputPath, output, "utf8");
  console.log(JSON.stringify({
    verifiedServiceScore: result.verifiedServiceScore,
    invalidRetainedAntennas: validated.invalidRetainedAntennaCount,
    snappedAntennas: validated.validAntennas.filter((antenna) => antenna.snapped).length,
    duplicateAntennas: validated.validAntennas.filter(
      (antenna) => antenna.duplicateOfInputIndex !== undefined,
    ).length,
    elapsedSeconds: (performance.now() - started) / 1000,
    output: outputPath,
  }));
}, 86_400_000);

