// Typed port of the are-we-fast-yet benchmark of the same name (MIT license; see ../LICENSE.md).
import { Benchmark } from './benchmark.ts';

class Sieve extends Benchmark {
  sieve(flags: boolean[], size: number): number {
    let primeCount = 0;
    for (let i = 2; i <= size; i += 1) {
      if (flags[i - 1]) {
        primeCount += 1;
        let k = i + i;
        while (k <= size) {
          flags[k - 1] = false;
          k += i;
        }
      }
    }
    return primeCount;
  }

  benchmark(): number {
    const flags = new Array<boolean>(5000).fill(true);
    return this.sieve(flags, 5000);
  }

  verifyResult(result: number): boolean {
    return 669 === result;
  }
}

export const newInstance = (): Benchmark => new Sieve();
