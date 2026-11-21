// Cyclic structures whose references move while the cycle collector may
// be mid-check (the ownership build checks incrementally): pieces held only
// by locals, moved between structures, cut out and put back, and garbage
// cycles alongside so checks keep running. Any live box freed by mistake
// shows as a wrong sum or a crash.

class Node {
  next: Node | null = null;
  prev: Node | null = null;
  kids: Node[] = [];
  parent: Node | null = null;
  value: number;
  constructor(value: number) {
    this.value = value;
  }
}

function ring(n: number, base: number): Node {
  const first = new Node(base);
  let last = first;
  for (let i = 1; i < n; i++) {
    const x = new Node(base + i);
    x.prev = last;
    last.next = x;
    last = x;
  }
  last.next = first;
  first.prev = last;
  return first;
}

function ringSum(r: Node): number {
  let s = r.value;
  for (let x = r.next; x !== null && x !== r; x = x.next) s += x.value;
  return s;
}

function tree(depth: number, parent: Node | null, base: number): Node {
  const n = new Node(base);
  n.parent = parent;
  if (depth > 0) for (let i = 0; i < 3; i++) n.kids.push(tree(depth - 1, n, base * 3 + i));
  return n;
}

function treeSum(n: Node): number {
  let s = n.value;
  for (const k of n.kids) s += treeSum(k);
  return s;
}

let seed = 7;
function rand(n: number): number {
  seed = (seed * 1103515245 + 12345) % 2147483648;
  return seed % n;
}

const rings: Node[] = [];
for (let i = 0; i < 20; i++) rings.push(ring(50, i * 1000));
const forest: Node[] = [];
for (let i = 0; i < 10; i++) forest.push(tree(4, null, i + 1));
const byName = new Map<string, Node>();
let total = 0;

for (let step = 0; step < 3000; step++) {
  // Garbage cycles, so the collector keeps checking.
  for (let g = 0; g < 5; g++) ring(20, -1);
  tree(3, null, 1);

  // Hold a ring only from a local while its slot is reused, then put it back.
  const i = rand(rings.length);
  const held = rings[i];
  rings[i] = ring(5, step);
  for (let k = 0; k < 3; k++) ring(10, -2);
  total += ringSum(held) % 1000;
  rings[i] = held;

  // Cut a node out of a ring, keep it in a local, splice it into another.
  const a = rings[rand(rings.length)];
  const b = rings[rand(rings.length)];
  if (a !== b && a.next !== a) {
    const x = a.next!;
    a.next = x.next;
    x.next!.prev = a;
    x.next = null;
    x.prev = null;
    tree(2, null, 2);
    x.next = b.next;
    x.prev = b;
    b.next!.prev = x;
    b.next = x;
  }

  // Move a subtree between trees (its parent pointer follows).
  const t = forest[rand(forest.length)];
  const u = forest[rand(forest.length)];
  if (t.kids.length > 0 && t !== u) {
    const k = t.kids.pop()!;
    ring(15, -3);
    k.parent = u;
    u.kids.push(k);
  }

  // A cycle kept only by a closure and a map entry.
  const c = ring(4, step);
  const f = () => c.value + c.next!.value;
  byName.set("k" + (step % 7), c);
  total += f() % 100;

  // Replace a whole tree; the old one becomes garbage.
  if (step % 50 === 0) forest[rand(forest.length)] = tree(4, null, step);

  if (step % 500 === 0) {
    let s = 0;
    for (const r of rings) s += ringSum(r);
    for (const tr of forest) s += treeSum(tr);
    for (const [, v] of byName) s += ringSum(v);
    console.log(step, s % 1000003, total % 1000003);
  }
}

let s = 0;
for (const r of rings) s += ringSum(r);
for (const tr of forest) s += treeSum(tr);
console.log("end", s % 1000003, total % 1000003);
