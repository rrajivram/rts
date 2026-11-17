// Native code calling native code: virtual methods with several
// implementations, and calls through function values (closures).

abstract class Shape {
  constructor(public name: string) {}
  abstract area(): number;
  describe(): string {
    return this.name + " " + this.area().toFixed(2);
  }
  scaled(k: number): Shape | null {
    return null;
  }
}

class Square extends Shape {
  constructor(public side: number) {
    super("square");
  }
  area(): number {
    return this.side * this.side;
  }
  scaled(k: number): Shape | null {
    return new Square(this.side * k);
  }
}

class Circle extends Shape {
  constructor(public r: number) {
    super("circle");
  }
  area(): number {
    return 3.14159 * this.r * this.r;
  }
}

// A subclass of a subclass, overriding again.
class Ring extends Circle {
  constructor(r: number, public inner: number) {
    super(r);
    this.name = "ring";
  }
  area(): number {
    return super.area() - 3.14159 * this.inner * this.inner;
  }
  scaled(k: number): Shape | null {
    if (k <= 0) throw new RangeError("bad scale " + k);
    return new Ring(this.r * k, this.inner * k);
  }
}

function totalArea(shapes: Shape[]): number {
  let t = 0;
  for (let i = 0; i < shapes.length; i++) t += shapes[i].area();
  return t;
}

function grow(shapes: Shape[], k: number): string {
  let out = "";
  for (const s of shapes) {
    const g = s.scaled(k);
    out += g === null ? "- " : g.describe() + "; ";
  }
  return out;
}

const shapes: Shape[] = [new Square(2), new Circle(1), new Ring(2, 1), new Square(3)];
let t = 0;
for (let i = 0; i < 200; i++) t += totalArea(shapes);
console.log(t.toFixed(3));
console.log(grow(shapes, 2));
try {
  grow(shapes, 0);
} catch (e) {
  console.log((e as Error).message);
}
// Calls through function values.
type NumFn = (x: number) => number;

function apply(f: NumFn, xs: number[]): number {
  let s = 0;
  for (let i = 0; i < xs.length; i++) s += f(xs[i]);
  return s;
}

function makeAdder(k: number): NumFn {
  return (x) => x + k;
}

function makeCounter(): () => number {
  let n = 0;
  return () => ++n;
}

const xs = [1, 2, 3, 4];
let acc = 0;
for (let i = 0; i < 100; i++) acc += apply((x) => x * 2, xs) + apply(makeAdder(i), xs);
console.log(acc);

const c = makeCounter();
let last = 0;
for (let i = 0; i < 50; i++) last = c();
console.log(last);

// Not compiled with these kinds: a function taking `any`, a bound
// function, a built-in.
const loose = function (x: any): any {
  return typeof x === "number" ? x + 0.5 : "?";
} as unknown as NumFn;
console.log(apply(loose, xs));
const bound = function (x: number): number {
  return x * (this as unknown as { k: number }).k;
}.bind({ k: 10 });
console.log(apply(bound, xs), apply(Math.sqrt, [4, 9]));

// Objects and void results through function values.
type Make = (s: number) => Square;
function build(m: Make, n: number): number {
  let s = 0;
  for (let i = 1; i <= n; i++) s += m(i).area();
  return s;
}
console.log(build((s) => new Square(s), 5));

const log: string[] = [];
function each(xs: number[], f: (x: number) => void): void {
  for (const x of xs) f(x);
}
each(xs, (x) => {
  log.push("v" + x);
});
console.log(log.join(","));

// An exception thrown through a function value.
function check(f: NumFn): string {
  try {
    return "ok " + f(1);
  } catch (e) {
    return "caught " + (e as Error).message;
  }
}
console.log(
  check((x) => x),
  check((x): number => {
    throw new Error("no " + x);
  }),
);

// Generic callbacks: the call site passes values, the callee takes
// numbers or objects.
class Box<T> {
  items: T[] = [];
  add(x: T): Box<T> {
    this.items.push(x);
    return this;
  }
  each(f: (x: T) => void): void {
    for (let i = 0; i < this.items.length; i++) f(this.items[i]);
  }
  fold<R>(init: R, f: (acc: R, x: T) => R): R {
    let r = init;
    for (const x of this.items) r = f(r, x);
    return r;
  }
}
const nums = new Box<number>().add(1).add(2).add(3);
let sum = 0;
for (let i = 0; i < 100; i++) nums.each((x: number) => (sum += x));
console.log(sum, nums.fold(0, (a: number, x: number) => a + x * x));
const sq = new Box<Square>().add(new Square(1)).add(new Square(2));
console.log(sq.fold("", (a: string, s: Square) => a + s.area() + " "));
const nulls = new Box<Square | null>().add(null).add(new Square(5));
console.log(nulls.fold(0, (a: number, s: Square | null) => a + (s === null ? -1 : s.side)));

// Calls by name through an interface: several classes at one site, a
// plain object, an own property shadowing a method.
interface Comparable<T> {
  compareTo(other: T): number;
}
class Num implements Comparable<Num> {
  constructor(public v: number) {}
  compareTo(o: Num): number {
    return this.v - o.v;
  }
}
class Rev extends Num {
  compareTo(o: Num): number {
    return o.v - this.v;
  }
}
function maxOf<T extends Comparable<T>>(xs: T[]): T {
  let best = xs[0];
  for (let i = 1; i < xs.length; i++) if (xs[i].compareTo(best) > 0) best = xs[i];
  return best;
}
let picks = "";
for (let r = 0; r < 50; r++) {
  const a = maxOf([new Num(3), new Num(9), new Num(4)]);
  const b = maxOf([new Rev(3), new Rev(9), new Rev(4)]);
  if (r === 49) picks = a.v + " " + b.v;
}
console.log(picks);
const plain = { v: 5, compareTo: (o: { v: number }) => 100 - o.v };
const shadow = new Num(1);
(shadow as any).compareTo = (o: Num) => -1000;
// The site cached Num: an instance with an own compareTo has another shape.
console.log(maxOf([plain, { v: 2, compareTo: plain.compareTo }]).v, maxOf([new Num(0), shadow]).v, maxOf([new Num(7), new Num(8)]).v);
interface Speaker {
  speak(times: number): string;
}
class Dog implements Speaker {
  speak(times: number): string {
    return "woof".repeat(times);
  }
}
function talk(s: Speaker, n: any): string {
  return s.speak(n);
}
let said = "";
for (let i = 0; i < 20; i++) said = talk(new Dog(), 2);
console.log(said);
