// Typed port of the are-we-fast-yet benchmark of the same name (MIT license; see ../LICENSE.md).
import { Benchmark } from './benchmark.ts';
import { Random } from './som.ts';

class Storage extends Benchmark {
  count = 0;

  benchmark(): number {
    const random = new Random();
    this.count = 0;
    this.buildTreeDepth(7, random);
    return this.count;
  }

  verifyResult(result: number): boolean {
    return 5461 === result;
  }

  buildTreeDepth(depth: number, random: Random): unknown[] {
    this.count += 1;
    if (depth === 1) {
      return new Array<unknown>((random.next() % 10) + 1);
    }
    const arr = new Array<unknown>(4);
    for (let i = 0; i < 4; i += 1) {
      arr[i] = this.buildTreeDepth(depth - 1, random);
    }
    return arr;
  }
}

export const newInstance = (): Benchmark => new Storage();
