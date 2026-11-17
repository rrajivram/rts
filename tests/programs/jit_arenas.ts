// Scope arenas in compiled code: objects that never leave a block are
// allocated in its arena and freed when the block ends, on every way out
// (falling off the end, break, continue, return, an exception).

class Vec2 {
  constructor(
    public x: number,
    public y: number,
  ) {}
  plus(o: Vec2): Vec2 {
    return new Vec2(this.x + o.x, this.y + o.y);
  }
  times(k: number): Vec2 {
    return new Vec2(this.x * k, this.y * k);
  }
  len2(): number {
    return this.x * this.x + this.y * this.y;
  }
}

// Temporaries in a loop body, from callees that return fresh objects.
function pathLength(n: number): number {
  let total = 0;
  for (let i = 0; i < n; i++) {
    const a = new Vec2(i, i + 1);
    const b = a.plus(new Vec2(1, 1)).times(0.5);
    total += b.len2();
  }
  return total;
}

// Leaving the block early.
function firstFar(n: number, limit: number): number {
  for (let i = 0; i < n; i++) {
    const v = new Vec2(i, i * 2);
    const w = v.times(2);
    if (w.len2() > limit) return i;
    if (i % 3 === 0) continue;
    if (w.x > 1000) break;
  }
  return -1;
}

// An exception out of a block with an arena.
function mayThrow(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const v = new Vec2(i, 1).plus(new Vec2(1, i));
    if (v.x > 5) throw new RangeError("too far at " + i);
    s += v.y;
  }
  return s;
}

// Arrays as temporaries, walked with for-of, left with break.
function sumSmall(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const xs = [i, i + 1, i + 2, i + 3];
    for (const x of xs) {
      if (x > i + 2) break;
      s += x;
    }
  }
  return s;
}

// Nested blocks, labeled exits.
function grid(n: number): number {
  let hits = 0;
  outer: for (let i = 0; i < n; i++) {
    const row = new Vec2(i, 0);
    for (let j = 0; j < n; j++) {
      const cell = row.plus(new Vec2(0, j));
      if (cell.len2() > 50) continue outer;
      if (cell.x === 7) break outer;
      hits++;
    }
  }
  return hits;
}

// Objects that escape stay on the heap.
let kept: Vec2[] = [];
function keepSome(n: number): number {
  for (let i = 0; i < n; i++) {
    const v = new Vec2(i, i);
    if (i % 4 === 0) kept.push(v);
  }
  return kept.length;
}

let total = 0;
for (let r = 0; r < 50; r++) total += pathLength(100);
console.log(total);
console.log(firstFar(100, 500), firstFar(5, 1e9), sumSmall(100), grid(10));
try {
  mayThrow(10);
} catch (e) {
  console.log((e as Error).message);
}
console.log(mayThrow(4));
console.log(keepSome(20), kept[3].x, kept.map((v) => v.y).join(","));
