// Class instances in compiled code: fields, methods, `new`, null checks,
// reference counts (objects overwritten in fields and variables).

class Node {
  value: number;
  next: Node | null;
  constructor(value: number, next: Node | null) {
    this.value = value;
    this.next = next;
  }
  sum(): number {
    let s = 0;
    let n: Node | null = this;
    while (n !== null) {
      s += n.value;
      n = n.next;
    }
    return s;
  }
  nth(i: number): Node | null {
    let n: Node | null = this;
    while (i > 0 && n !== null) {
      n = n.next;
      i--;
    }
    return n;
  }
}

function build(k: number): Node | null {
  let head: Node | null = null;
  for (let i = 0; i < k; i++) {
    head = new Node(i, head);
  }
  return head;
}

function length(n: Node | null): number {
  let c = 0;
  while (n) {
    c++;
    n = n.next;
  }
  return c;
}

/// Drops every other node: fields overwritten while the list is walked.
function thin(head: Node): number {
  let n: Node | null = head;
  while (n !== null && n.next !== null) {
    n.next = n.next.next;
    n = n.next;
  }
  return length(head);
}

class Vec {
  x: number;
  y: number;
  constructor(x: number, y: number) {
    this.x = x;
    this.y = y;
  }
  plus(o: Vec): Vec {
    return new Vec(this.x + o.x, this.y + o.y);
  }
  scale(k: number): Vec {
    return new Vec(this.x * k, this.y * k);
  }
  len(): number {
    return Math.sqrt(this.x * this.x + this.y * this.y);
  }
}

function walk(steps: number): number {
  let at = new Vec(0, 0);
  for (let i = 0; i < steps; i++) {
    at = at.plus(new Vec(i % 3, i % 5).scale(0.5));
  }
  return Math.round(at.len());
}

class Shape {
  area(): number {
    return 0;
  }
}
class Square extends Shape {
  side: number;
  constructor(side: number) {
    super();
    this.side = side;
  }
  area(): number {
    return this.side * this.side;
  }
}
class Circle extends Shape {
  r: number;
  constructor(r: number) {
    super();
    this.r = r;
  }
  area(): number {
    return 3 * this.r * this.r;
  }
}

// Virtual (several targets): stays interpreted; single target: compiled.
function areas(a: Shape, b: Shape): number {
  return a.area() + b.area();
}
function squareArea(s: Square): number {
  return s.area();
}

class Counter {
  count: number = 0;
  flag: boolean = false;
  bump(): boolean {
    this.count++;
    this.count += 2;
    this.flag = !this.flag;
    return this.flag;
  }
}

function nextValue(n: Node): number {
  return n.next!.value;
}

function same(a: Node | null, b: Node | null): boolean {
  return a === b;
}

function pick(c: boolean, a: Node, b: Node): Node {
  return c ? a : b;
}

const list = build(100);
console.log(list!.sum(), length(list), list!.nth(10)!.value, list!.nth(1000));
console.log(thin(list!), length(list));
console.log(walk(1000));
console.log(areas(new Square(3), new Circle(2)), squareArea(new Square(5)));
const c = new Counter();
let t = 0;
for (let i = 0; i < 5; i++) {
  if (c.bump()) t++;
}
console.log(c.count, c.flag, t);
console.log(nextValue(new Node(1, new Node(2, null))));
try {
  console.log(nextValue(new Node(1, null)));
} catch (e) {
  console.log((e as Error).name, (e as Error).message.startsWith("Cannot read properties of null"));
}
const n1 = new Node(1, null);
console.log(same(n1, n1), same(n1, new Node(1, null)), same(null, null), same(n1, null));
console.log(pick(true, n1, new Node(2, null)).value, pick(false, n1, new Node(2, null)).value);
let deep = 0;
function recurse(n: Node): number {
  return recurse(new Node(n.value + 1, n));
}
try {
  recurse(new Node(0, null));
} catch (e) {
  deep = 1;
  console.log((e as Error).name);
}
console.log(deep);
