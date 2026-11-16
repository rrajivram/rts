// Typed port of the are-we-fast-yet benchmark of the same name (MIT license; see ../LICENSE.md).
import { Benchmark } from './benchmark.ts';

class Queens extends Benchmark {
  freeMaxs: boolean[] = [];
  freeRows: boolean[] = [];
  freeMins: boolean[] = [];
  queenRows: number[] = [];

  benchmark(): boolean {
    let result = true;
    for (let i = 0; i < 10; i += 1) {
      result = result && this.queens();
    }
    return result;
  }

  verifyResult(result: boolean): boolean {
    return result;
  }

  queens(): boolean {
    this.freeRows = new Array<boolean>(8).fill(true);
    this.freeMaxs = new Array<boolean>(16).fill(true);
    this.freeMins = new Array<boolean>(16).fill(true);
    this.queenRows = new Array<number>(8).fill(-1);
    return this.placeQueen(0);
  }

  placeQueen(c: number): boolean {
    for (let r = 0; r < 8; r += 1) {
      if (this.getRowColumn(r, c)) {
        this.queenRows[r] = c;
        this.setRowColumn(r, c, false);
        if (c === 7) {
          return true;
        }
        if (this.placeQueen(c + 1)) {
          return true;
        }
        this.setRowColumn(r, c, true);
      }
    }
    return false;
  }

  getRowColumn(r: number, c: number): boolean {
    return this.freeRows[r] && this.freeMaxs[c + r] && this.freeMins[c - r + 7];
  }

  setRowColumn(r: number, c: number, v: boolean): void {
    this.freeRows[r] = v;
    this.freeMaxs[c + r] = v;
    this.freeMins[c - r + 7] = v;
  }
}

export const newInstance = (): Benchmark => new Queens();
