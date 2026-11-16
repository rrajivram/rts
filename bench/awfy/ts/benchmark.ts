// Typed port of are-we-fast-yet's benchmark.js (MIT license; see ../LICENSE.md).

export abstract class Benchmark {
  innerBenchmarkLoop(innerIterations: number): boolean {
    for (let i = 0; i < innerIterations; i += 1) {
      if (!this.verifyResult(this.benchmark())) {
        return false;
      }
    }
    return true;
  }

  benchmark(): unknown {
    throw new Error('subclass responsibility');
  }

  verifyResult(_result: unknown): boolean {
    throw new Error('subclass responsibility');
  }
}
