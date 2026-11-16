// Typed port of the are-we-fast-yet benchmark of the same name (MIT license; see ../LICENSE.md).
import { Benchmark } from './benchmark.ts';

class TowersDisk {
  size: number;
  next: TowersDisk | null;

  constructor(size: number) {
    this.size = size;
    this.next = null;
  }
}

class Towers extends Benchmark {
  piles: (TowersDisk | null)[] = [];
  movesDone = 0;

  benchmark(): number {
    this.piles = [null, null, null];
    this.buildTowerAt(0, 13);
    this.movesDone = 0;
    this.moveDisks(13, 0, 1);
    return this.movesDone;
  }

  verifyResult(result: number): boolean {
    return 8191 === result;
  }

  pushDisk(disk: TowersDisk, pile: number): void {
    const top = this.piles[pile];
    if (top && disk.size >= top.size) {
      throw new Error('Cannot put a big disk on a smaller one');
    }
    disk.next = top;
    this.piles[pile] = disk;
  }

  popDiskFrom(pile: number): TowersDisk {
    const top = this.piles[pile];
    if (top === null) {
      throw new Error('Attempting to remove a disk from an empty pile');
    }
    this.piles[pile] = top.next;
    top.next = null;
    return top;
  }

  moveTopDisk(fromPile: number, toPile: number): void {
    this.pushDisk(this.popDiskFrom(fromPile), toPile);
    this.movesDone += 1;
  }

  buildTowerAt(pile: number, disks: number): void {
    for (let i = disks; i >= 0; i -= 1) {
      this.pushDisk(new TowersDisk(i), pile);
    }
  }

  moveDisks(disks: number, fromPile: number, toPile: number): void {
    if (disks === 1) {
      this.moveTopDisk(fromPile, toPile);
    } else {
      const otherPile = (3 - fromPile) - toPile;
      this.moveDisks(disks - 1, fromPile, otherPile);
      this.moveTopDisk(fromPile, toPile);
      this.moveDisks(disks - 1, otherPile, toPile);
    }
  }
}

export const newInstance = (): Benchmark => new Towers();
