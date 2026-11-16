// Typed port of the are-we-fast-yet benchmark of the same name (MIT license; see ../LICENSE.md).
import { Benchmark } from './benchmark.ts';

class Permute extends Benchmark {
  count = 0;
  v: number[] = [];

  benchmark(): number {
    this.count = 0;
    this.v = new Array<number>(6).fill(0);
    this.permute(6);
    return this.count;
  }

  verifyResult(result: number): boolean {
    return result === 8660;
  }

  permute(n: number): void {
    this.count += 1;
    if (n !== 0) {
      const n1 = n - 1;
      this.permute(n1);
      for (let i = n1; i >= 0; i -= 1) {
        this.swap(n1, i);
        this.permute(n1);
        this.swap(n1, i);
      }
    }
  }

  swap(i: number, j: number): void {
    const tmp = this.v[i];
    this.v[i] = this.v[j];
    this.v[j] = tmp;
  }
}

export const newInstance = (): Benchmark => new Permute();
