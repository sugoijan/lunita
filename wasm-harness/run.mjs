// Runs every limit case of tests/limits.rs through the wasm harness, in a fresh instance of the
// module each, and checks the outcome, that nothing traps and that memory grows by at most
// 32 MiB + 4 MiB.
//
//   cargo test --test limits -- --ignored export_cases
//   (cd wasm-harness && cargo build --release --target wasm32-unknown-unknown)
//   node wasm-harness/run.mjs [case ids, comma-separated]
//
// HARNESS_WASM=<path> runs another build of the module, such as one built with
// `CARGO_PROFILE_RELEASE_OPT_LEVEL=z` (the game's profile), HARNESS_REPORT=<path> writes the
// table there, and HARNESS_RUNS=<n> runs each case n times for its times (see below).
//
// Step times are measured from JavaScript around each `step()` call (reported, no gate), with the
// worst time per fuel in steps over 0.5 ms, and the compile time per unit of the compiler's work.
// The table is printed and written to target/wasm-limits-report.md.

import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { performance } from 'node:perf_hooks';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const wasmPath =
  process.env.HARNESS_WASM ??
  join(root, 'wasm-harness/target/wasm32-unknown-unknown/release/lunita_wasm_harness.wasm');
const module = new WebAssembly.Module(readFileSync(wasmPath));
if (WebAssembly.Module.imports(module).length !== 0) {
  throw new Error('the harness module has imports');
}

const MIB = 1 << 20;
// The memory limit, plus room for the allocator's fragmentation (wasm memory never shrinks).
const MAX_GROWTH = 32 * MIB + 4 * MIB;
const HOST_STOP_MS = 2000;

// cases.txt: a line `<id> <expected outcomes> <length>` before each source.
function readCases() {
  const data = readFileSync(join(root, 'target/limit-cases/cases.txt'));
  const cases = [];
  let pos = 0;
  while (pos < data.length) {
    const end = data.indexOf(10, pos);
    const [id, expected, length] = data.subarray(pos, end).toString().split(' ');
    const start = end + 1;
    cases.push({
      id,
      expected: expected.split(','),
      source: data.subarray(start, start + Number(length)),
    });
    pos = start + Number(length) + 1;
  }
  return cases;
}

// Room for the times of every step of a run (the most any case takes in 2 s is about 450,000).
const MAX_SAMPLES = 1 << 20;

// Runs a case once, writing the time (ms) and fuel of each step into `times` and `fuels`.
function run(c, times, fuels) {
  const { exports } = new WebAssembly.Instance(module, {});
  const memory = exports.memory;
  const ptr = exports.source_buffer(c.source.length);
  new Uint8Array(memory.buffer, ptr, c.source.length).set(c.source);
  // The baseline is taken with the source in memory, as natively.
  const baseline = memory.buffer.byteLength;
  const record = {
    outcome: 'compile_error',
    steps: 0,
    fuel: 0,
    growth: 0,
    compileMs: 0,
    compileWork: 0,
    trap: null,
  };
  try {
    let t = performance.now();
    const started = exports.start();
    record.compileMs = performance.now() - t;
    record.compileWork = exports.compile_work();
    if (started === 2) {
      record.outcome = 'limit';
    } else if (started === 0) {
      const begin = performance.now();
      for (;;) {
        t = performance.now();
        const status = exports.step();
        const now = performance.now();
        const fuel = exports.step_fuel();
        if (record.steps < MAX_SAMPLES) {
          times[record.steps] = now - t;
          fuels[record.steps] = fuel;
        }
        record.steps += 1;
        record.fuel += fuel;
        if (status !== 0) {
          record.outcome = ['', 'ok', 'error', 'limit'][status];
          break;
        }
        if (now - begin >= HOST_STOP_MS) {
          record.outcome = 'host_stop';
          break;
        }
      }
    }
  } catch (e) {
    record.outcome = 'trap';
    record.trap = String(e);
  }
  record.growth = memory.buffer.byteLength - baseline;
  return record;
}

// Each step does the same work in every run, so with HARNESS_RUNS=n each case runs n times and each
// step (and the compilation) keeps its least time over the runs, which leaves out the time other
// processes and the engine took from it. The memory and outcome are those of the first run.
const RUNS = Math.max(1, Number(process.env.HARNESS_RUNS ?? 1));
const only = process.argv[2]?.split(',');
const rows = [
  '| Case | Outcome | Steps | Fuel | Memory growth (MiB) | Compile (ms) | Longest step (ms) | ' +
    'Compile work | Compile µs/work | Worst µs/fuel |',
  '|---|---|---|---|---|---|---|---|---|---|',
];
const failures = [];
const least = new Float64Array(MAX_SAMPLES);
const leastFuels = new Int32Array(MAX_SAMPLES);
const times = new Float64Array(MAX_SAMPLES);
const fuels = new Int32Array(MAX_SAMPLES);
for (const c of readCases()) {
  if (only && !only.includes(c.id)) continue;
  const r = run(c, least, leastFuels);
  let samples = Math.min(r.steps, MAX_SAMPLES);
  for (let i = 1; i < RUNS; i++) {
    const again = run(c, times, fuels);
    r.compileMs = Math.min(r.compileMs, again.compileMs);
    for (let j = 0; j < Math.min(samples, again.steps); j++) {
      if (fuels[j] === leastFuels[j]) least[j] = Math.min(least[j], times[j]);
    }
  }
  let longestMs = 0;
  let worstUsPerFuel = 0;
  for (let j = 0; j < samples; j++) {
    longestMs = Math.max(longestMs, least[j]);
    if (least[j] > 0.5) {
      worstUsPerFuel = Math.max(worstUsPerFuel, (least[j] * 1e3) / Math.max(leastFuels[j], 1));
    }
  }
  rows.push(
    `| ${c.id} | ${r.outcome.replace('_', ' ')} | ${r.steps} | ${r.fuel} | ` +
      `${(r.growth / MIB).toFixed(2)} | ${r.compileMs.toFixed(2)} | ${longestMs.toFixed(2)} | ` +
      `${r.compileWork} | ${((r.compileMs * 1e3) / Math.max(r.compileWork, 1)).toFixed(3)} | ` +
      `${worstUsPerFuel.toFixed(3)} |`,
  );
  if (r.trap) failures.push(`${c.id}: trap: ${r.trap}`);
  else if (!c.expected.includes(r.outcome)) {
    failures.push(`${c.id}: outcome ${r.outcome}, expected one of ${c.expected.join(', ')}`);
  }
  if (r.growth > MAX_GROWTH) failures.push(`${c.id}: memory grew by ${r.growth} bytes`);
}

const report = rows.join('\n') + '\n';
console.log(report);
writeFileSync(process.env.HARNESS_REPORT ?? join(root, 'target/wasm-limits-report.md'), report);
if (failures.length) {
  console.error('failures:\n' + failures.join('\n'));
  process.exit(1);
}
