// Typed port of are-we-fast-yet's harness.js and run.js (MIT license; see ../LICENSE.md).
//   harness.ts [benchmark] [num-iterations [inner-iter]]

import type { Benchmark } from './benchmark.ts';
import * as Bounce from './bounce.ts';
import * as CD from './cd.ts';
import * as DeltaBlue from './deltablue.ts';
import * as Havlak from './havlak.ts';
import * as Json from './json.ts';
import * as List from './list.ts';
import * as Mandelbrot from './mandelbrot.ts';
import * as NBody from './nbody.ts';
import * as Permute from './permute.ts';
import * as Queens from './queens.ts';
import * as Richards from './richards.ts';
import * as Sieve from './sieve.ts';
import * as Storage from './storage.ts';
import * as Towers from './towers.ts';

function load(name: string): Benchmark {
  switch (name) {
    case 'Bounce': return Bounce.newInstance();
    case 'CD': return CD.newInstance();
    case 'DeltaBlue': return DeltaBlue.newInstance();
    case 'Havlak': return Havlak.newInstance();
    case 'Json': return Json.newInstance();
    case 'List': return List.newInstance();
    case 'Mandelbrot': return Mandelbrot.newInstance();
    case 'NBody': return NBody.newInstance();
    case 'Permute': return Permute.newInstance();
    case 'Queens': return Queens.newInstance();
    case 'Richards': return Richards.newInstance();
    case 'Sieve': return Sieve.newInstance();
    case 'Storage': return Storage.newInstance();
    case 'Towers': return Towers.newInstance();
  }
  throw new Error(`unknown benchmark ${name}`);
}

class Run {
  name: string;
  numIterations = 1;
  innerIterations = 1;
  total = 0;

  constructor(name: string) {
    this.name = name;
  }

  measure(bench: Benchmark): void {
    const startTime = process.hrtime();
    if (!bench.innerBenchmarkLoop(this.innerIterations)) {
      throw Error('Benchmark failed with incorrect result');
    }
    const diff = process.hrtime(startTime);
    const runTime = ((diff[0] * 1e9 + diff[1]) / 1000) | 0;
    process.stdout.write(`${this.name}: iterations=1 runtime: ${Math.round(runTime)}us\n`);
    this.total += runTime;
  }

  runBenchmark(): void {
    process.stdout.write(`Starting ${this.name} benchmark ...\n`);
    const bench = load(this.name);
    for (let i = 0; i < this.numIterations; i += 1) {
      this.measure(bench);
    }
    process.stdout.write(`${this.name}: iterations=${this.numIterations} average: ${Math.round(this.total / this.numIterations)}us total: ${Math.round(this.total)}us\n\n`);
    process.stdout.write('\n');
  }
}

const args = process.argv;
if (args.length < 3) {
  process.stdout.write('harness.ts [benchmark] [num-iterations [inner-iter]]\n');
  process.exit(1);
}
const run = new Run(args[2]);
if (args.length > 3) {
  run.numIterations = parseInt(args[3], 10);
  if (args.length > 4) {
    run.innerIterations = parseInt(args[4], 10);
  }
}
run.runBenchmark();
process.stdout.write(`Total Runtime: ${run.total}us\n`);
