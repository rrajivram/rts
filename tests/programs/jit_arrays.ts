// Arrays and `throw` in compiled code.

class Disk {
  size: number;
  next: Disk | null = null;
  constructor(size: number) {
    this.size = size;
  }
}

class Piles {
  piles: (Disk | null)[] = [null, null, null];
  moves = 0;

  push(disk: Disk, pile: number): void {
    const top = this.piles[pile];
    if (top && disk.size >= top.size) {
      throw new Error('Cannot put a big disk on a smaller one');
    }
    disk.next = top;
    this.piles[pile] = disk;
  }

  pop(pile: number): Disk {
    const top = this.piles[pile];
    if (top === null) {
      throw new Error('Attempting to remove a disk from an empty pile');
    }
    this.piles[pile] = top.next;
    top.next = null;
    return top;
  }

  move(from: number, to: number): void {
    this.push(this.pop(from), to);
    this.moves += 1;
  }

  hanoi(n: number, from: number, to: number): void {
    if (n === 1) {
      this.move(from, to);
    } else {
      const other = 3 - from - to;
      this.hanoi(n - 1, from, other);
      this.move(from, to);
      this.hanoi(n - 1, other, to);
    }
  }
}

function sieve(flags: boolean[], size: number): number {
  let count = 0;
  for (let i = 2; i <= size; i += 1) {
    if (flags[i - 1]) {
      count += 1;
      let k = i + i;
      while (k <= size) {
        flags[k - 1] = false;
        k += i;
      }
    }
  }
  return count;
}

function sum(xs: number[]): number {
  let s = 0;
  for (let i = 0; i < xs.length; i++) {
    s += xs[i];
  }
  return s;
}

function squares(n: number): number[] {
  const out: number[] = [];
  for (let i = 0; i < n; i++) {
    out.push(i * i);
  }
  return out;
}

function swap(a: number[], i: number, j: number): void {
  const t = a[i];
  a[i] = a[j];
  a[j] = t;
}

function bump(a: number[]): number {
  a[0] += 5;
  a[1]++;
  return a[0] + a[1];
}

function grid(n: number): number[][] {
  const rows: number[][] = [];
  for (let i = 0; i < n; i++) {
    rows.push([i, i + 1, i + 2]);
  }
  return rows;
}

function cell(g: number[][], i: number, j: number): number {
  return g[i][j];
}

function disks(n: number): Disk[] {
  const out: Disk[] = [];
  for (let i = 0; i < n; i++) {
    out.push(new Disk(i));
  }
  return out;
}

function biggest(ds: Disk[]): number {
  let best = -1;
  for (let i = 0; i < ds.length; i++) {
    if (ds[i].size > best) {
      best = ds[i].size;
    }
  }
  return best;
}

function replace(ds: Disk[], i: number): number {
  ds[i] = new Disk(100 + i);
  return ds[i].size;
}

const p = new Piles();
for (let i = 9; i >= 0; i--) {
  p.push(new Disk(i), 0);
}
p.hanoi(10, 0, 1);
console.log(p.moves);
try {
  p.pop(0);
} catch (e) {
  console.log((e as Error).name, (e as Error).message);
}
try {
  p.push(new Disk(50), 1);
} catch (e) {
  console.log((e as Error).message);
}

const flags: boolean[] = [];
for (let i = 0; i < 1000; i++) flags.push(true);
console.log(sieve(flags, 1000));
const sq = squares(10);
console.log(sum(sq), sq.length);
swap(sq, 0, 9);
console.log(sq[0], sq[9], bump(sq), sq[0], sq[1]);
const g = grid(4);
console.log(cell(g, 3, 2), g.length);
const ds = disks(20);
console.log(biggest(ds), replace(ds, 3), ds[3].size, biggest(ds));

function edges(a: number[], ds: (Disk | null)[]): number {
  a[a.length] = 7;
  a[-1] = 3;
  a[1.5] = 4;
  a[a.length + 2] = 9;
  ds[0] = null;
  ds[ds.length] = new Disk(42);
  return a.length * 100 + ds.length;
}
const ea = [1, 2, 3];
const ed: (Disk | null)[] = [new Disk(1)];
console.log(edges(ea, ed), ea.length, ea[3], ea[6], ed[0], ed[1]!.size);
