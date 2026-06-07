// Property-based test for the pure capture-filter logic in filter.js.
//
// Property 12: Extension capture filtering correctness
//   "For any file size and file extension, the Browser_Extension filter SHALL
//    accept the download if and only if the size meets the minimum threshold
//    and the extension is not in the blacklist (or is in the whitelist when
//    configured)."
//   Validates: Requirement 6.5
//
// The repo has no JS test runner / PBT library (fast-check) installed, so this
// is a self-contained property test: it draws many randomized inputs from a
// deterministically-seeded PRNG and asserts the properties hold on every draw.
// Run it with:  node extension/filter.property.test.js
//
// **Validates: Requirements 6.5**

"use strict";

const assert = require("assert");
const filter = require("./filter.js");
const {
  shouldCapture,
  getExtension,
  normalizeList,
  sanitizeMinSize,
  MAX_MIN_SIZE_BYTES,
} = filter;

// ---------------------------------------------------------------------------
// Deterministic PRNG (mulberry32) so failures are reproducible.
// ---------------------------------------------------------------------------
function mulberry32(seed) {
  let a = seed >>> 0;
  return function () {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const SEED = 0x5f3759df;
const NUM_RUNS = 5000;
const rng = mulberry32(SEED);

function randInt(min, max) {
  return Math.floor(rng() * (max - min + 1)) + min;
}
function pick(arr) {
  return arr[randInt(0, arr.length - 1)];
}

// A pool of extensions to draw from (some overlap with lists, some not).
const EXT_POOL = [
  "mp4", "zip", "exe", "pdf", "jpg", "png", "iso", "mkv", "txt",
  "tar", "gz", "dmg", "msi", "rar", "7z", "mp3", "doc", "bin",
];

// Generate a filename with a known extension (or no extension sometimes).
function genFilename() {
  const r = rng();
  if (r < 0.15) return null; // no filename
  if (r < 0.25) return "noextfile"; // no extension
  if (r < 0.30) return ".hidden"; // dotfile -> no real extension
  const ext = pick(EXT_POOL);
  // Mix of casing and leading dots to exercise normalization.
  const cased = rng() < 0.5 ? ext.toUpperCase() : ext;
  return `download_${randInt(1, 999)}.${cased}`;
}

// Generate a size: numbers, zero, negatives, NaN, undefined, null, non-number.
function genSize() {
  const r = rng();
  if (r < 0.1) return null;
  if (r < 0.15) return undefined;
  if (r < 0.2) return NaN;
  if (r < 0.25) return "12345"; // wrong type
  if (r < 0.3) return -randInt(1, 1000000); // negative
  // Wide spread around the typical threshold range (0..100 MB).
  return randInt(0, 120 * 1024 * 1024);
}

function genList() {
  const r = rng();
  if (r < 0.5) return []; // empty (no restriction)
  const n = randInt(1, 5);
  const out = [];
  for (let i = 0; i < n; i++) {
    const ext = pick(EXT_POOL);
    out.push(rng() < 0.5 ? `.${ext.toUpperCase()}` : ext);
  }
  return out;
}

function genConfig() {
  const whitelist = genList();
  // Only sometimes add a blacklist; sometimes both (whitelist wins).
  const blacklist = rng() < 0.5 ? genList() : [];
  return {
    minSizeBytes: pick([
      0,
      1024,
      1048576,
      5 * 1048576,
      100 * 1024 * 1024,
      150 * 1024 * 1024, // above max, should clamp
      -5, // invalid, should fall back to default
      NaN, // invalid
    ]),
    whitelist,
    blacklist,
  };
}

function genPayload() {
  const filename = genFilename();
  const r = rng();
  const url =
    r < 0.4
      ? `https://example.com/path/file_${randInt(1, 99)}.${pick(EXT_POOL)}?q=1#frag`
      : r < 0.5
      ? null
      : "https://example.com/no-extension-here/";
  return { filesize: genSize(), filename, url };
}

// ---------------------------------------------------------------------------
// Property runner
// ---------------------------------------------------------------------------
let failures = 0;
let firstFailure = null;

function record(label, input, err) {
  failures++;
  if (!firstFailure) {
    firstFailure = { label, input, message: err && err.message };
  }
}

for (let i = 0; i < NUM_RUNS; i++) {
  const payload = genPayload();
  const config = genConfig();

  // (4) Totality: shouldCapture must never throw for arbitrary inputs.
  let result;
  try {
    result = shouldCapture(payload, config);
    assert.strictEqual(
      typeof result,
      "boolean",
      "shouldCapture must return a boolean"
    );
  } catch (err) {
    record("totality (never throws)", { payload, config }, err);
    continue;
  }

  const min = sanitizeMinSize(config.minSizeBytes);
  const size = payload.filesize;
  const sizeKnown = typeof size === "number" && isFinite(size);
  const ext = getExtension(payload.filename, payload.url);
  const whitelist = normalizeList(config.whitelist);
  const blacklist = normalizeList(config.blacklist);

  try {
    // (1) A known size below the minimum threshold is never captured.
    if (sizeKnown && size < min) {
      assert.strictEqual(
        result,
        false,
        "below-minimum size must never be captured"
      );
    }

    // (2) Non-empty whitelist: only extensions in it can be captured.
    if (result === true && whitelist.length > 0) {
      assert.ok(
        whitelist.includes(ext),
        "captured extension must be in the non-empty whitelist"
      );
    }

    // (3) Non-empty blacklist (no whitelist): blacklisted extensions never captured.
    if (whitelist.length === 0 && blacklist.length > 0) {
      if (blacklist.includes(ext)) {
        assert.strictEqual(
          result,
          false,
          "blacklisted extension must never be captured"
        );
      }
    }

    // Full iff-characterization of the predicate (the core of Property 12):
    //   capture  <=>  size ok  AND  extension passes the active list filter
    const sizeOk = !(sizeKnown && size < min);
    let extOk;
    if (whitelist.length > 0) {
      extOk = ext !== "" && whitelist.includes(ext);
    } else if (blacklist.length > 0) {
      extOk = !blacklist.includes(ext);
    } else {
      extOk = true;
    }
    assert.strictEqual(
      result,
      sizeOk && extOk,
      "shouldCapture must equal (sizeOk AND extOk)"
    );
  } catch (err) {
    record("filtering correctness", { payload, config, ext, min, result }, err);
  }
}

// ---------------------------------------------------------------------------
// Targeted edge-case checks (deterministic, not random).
// ---------------------------------------------------------------------------
try {
  // Unknown size + permissive config => captured.
  assert.strictEqual(
    shouldCapture({ filesize: null, filename: "a.zip" }, {}),
    true,
    "unknown size with permissive config should capture"
  );
  // Below threshold => skipped.
  assert.strictEqual(
    shouldCapture(
      { filesize: 10, filename: "a.zip" },
      { minSizeBytes: 1048576 }
    ),
    false,
    "tiny known size should be skipped"
  );
  // Whitelist excludes others.
  assert.strictEqual(
    shouldCapture(
      { filesize: 5e6, filename: "a.exe" },
      { minSizeBytes: 0, whitelist: ["zip"] }
    ),
    false,
    "extension outside whitelist should be skipped"
  );
  // Blacklist skips listed.
  assert.strictEqual(
    shouldCapture(
      { filesize: 5e6, filename: "a.exe" },
      { minSizeBytes: 0, blacklist: ["exe"] }
    ),
    false,
    "blacklisted extension should be skipped"
  );
  // Size clamps above 100 MB max.
  assert.strictEqual(
    sanitizeMinSize(150 * 1024 * 1024),
    MAX_MIN_SIZE_BYTES,
    "minSize should clamp to 100 MB"
  );
  // Totality on garbage inputs.
  shouldCapture(undefined, undefined);
  shouldCapture(null, null);
  shouldCapture({}, {});
  shouldCapture({ filesize: {}, filename: 42, url: [] }, { whitelist: 7 });
} catch (err) {
  record("edge-case", "deterministic", err);
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------
if (failures > 0) {
  console.error(
    `FAILED: ${failures}/${NUM_RUNS} runs failed (seed=${SEED}).`
  );
  console.error("First failing example:");
  console.error(JSON.stringify(firstFailure, null, 2));
  process.exit(1);
}

console.log(
  `PASSED: Property 12 (extension capture filtering) held over ${NUM_RUNS} randomized runs + edge cases (seed=${SEED}).`
);
process.exit(0);
