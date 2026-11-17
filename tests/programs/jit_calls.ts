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
