// Native code: switch, for-of, and throw of any value.

function kind(n: number): string {
  switch (n) {
    case 0:
      return "zero";
    case 1:
    case 2:
      return "small";
    default:
      return "big";
    case -1:
      return "negative";
  }
}

function fallthrough(s: string): number {
  let r = 0;
  switch (s) {
    case "a":
      r += 1;
    case "b":
      r += 10;
      break;
    case "c":
      r += 100;
    default:
      r += 1000;
  }
  return r;
}

let probes: string[] = [];
function probe(x: number): number {
  probes.push("p" + x);
  return x;
}

function lazyTests(n: number): string {
  // Tests run in order, only until one matches.
  switch (n) {
    case probe(1):
      return "one";
    case probe(2):
      return "two";
    case probe(3):
      return "three";
  }
  return "none";
}

function loopSwitch(xs: number[]): string {
  let out = "";
  outer: for (let i = 0; i < xs.length; i++) {
    switch (xs[i]) {
      case 0:
        continue;
      case 9:
        break outer;
      case 5:
        out += "five ";
        break;
      default:
        out += xs[i] + " ";
    }
    out += "| ";
  }
  return out;
}

function mixedSwitch(v: unknown): string {
  switch (v) {
    case 1:
      return "number one";
    case "1":
      return "string one";
    case true:
      return "true";
    case null:
      return "null";
    case undefined:
      return "undefined";
    default:
      return "other";
  }
}

class Item {
  constructor(
    public name: string,
    public n: number,
  ) {}
}

function total(items: Item[]): number {
  let t = 0;
  for (const it of items) t += it.n;
  return t;
}

function names(items: Item[], skip: string, stop: string): string {
  let s = "";
  for (const it of items) {
    if (it.name === skip) continue;
    if (it.name === stop) break;
    s += it.name;
  }
  return s;
}

function firstOver(items: Item[], min: number): Item | null {
  for (const it of items) if (it.n > min) return it;
  return null;
}

function growing(): number {
  // The length is read each time round.
  const xs = [1, 2, 3];
  let seen = 0;
  for (const x of xs) {
    seen++;
    if (x < 3) xs.push(x + 10);
  }
  return seen;
}

function iterables(): string {
  const parts: string[] = [];
  for (const ch of "héllo") parts.push(ch);
  const m = new Map<string, number>([
    ["a", 1],
    ["b", 2],
  ]);
  for (const [k, v] of m) parts.push(k + "=" + v);
  for (const v of new Set([3, 3, 4])) parts.push("" + v);
  return parts.join(",");
}

function captured(xs: number[]): number {
  const fs: (() => number)[] = [];
  for (const x of xs) fs.push(() => x * 2);
  let s = 0;
  for (const f of fs) s += f();
  return s;
}

function grid(rows: number[][]): number {
  let n = 0;
  rows: for (const row of rows) {
    for (const v of row) {
      if (v < 0) continue rows;
      n += v;
    }
  }
  return n;
}

function notIterable(xs: number[] | null): number {
  let n = 0;
  for (const x of xs as number[]) n += x;
  return n;
}

class ParseError extends Error {
  constructor(
    message: string,
    public at: number,
  ) {
    super(message);
  }
}

function parseDigit(s: string, at: number): number {
  const c = s.charCodeAt(at);
  if (c < 48 || c > 57) throw new ParseError("expected a digit at " + at + ", got '" + s[at] + "'", at);
  return c - 48;
}

function parseAll(s: string): number {
  let n = 0;
  for (let i = 0; i < s.length; i++) n = n * 10 + parseDigit(s, i);
  return n;
}

function throwAny(k: number): number {
  if (k === 0) throw "a string";
  if (k === 1) throw 42;
  if (k === 2) throw { code: k };
  return k;
}

console.log(kind(0), kind(2), kind(7), kind(-1));
console.log(fallthrough("a"), fallthrough("b"), fallthrough("c"), fallthrough("z"));
console.log(lazyTests(2), probes.join(" "));
probes = [];
console.log(lazyTests(7), probes.join(" "));
console.log(loopSwitch([1, 0, 5, 2, 9, 4]));
console.log(mixedSwitch(1), mixedSwitch("1"), mixedSwitch(true), mixedSwitch(null), mixedSwitch(undefined), mixedSwitch(2));

const items = [new Item("a", 1), new Item("b", 5), new Item("c", 9), new Item("d", 2)];
let t = 0;
for (let i = 0; i < 300; i++) t += total(items);
console.log(t, names(items, "b", "d"), firstOver(items, 4)?.name, firstOver(items, 40));
console.log(growing(), iterables(), captured([1, 2, 3]), grid([[1, 2], [3, -1, 100], [4]]));
try {
  notIterable(null);
} catch (e) {
  console.log((e as Error).constructor.name);
}

console.log(parseAll("1234"));
try {
  parseAll("12x4");
} catch (e) {
  const pe = e as ParseError;
  console.log(pe instanceof ParseError, pe.message, pe.at);
}
for (let k = 0; k < 4; k++) {
  try {
    console.log(throwAny(k));
  } catch (e) {
    console.log("caught", typeof e, JSON.stringify(e));
  }
}

// Static methods: `this` is the class.
class Shape {
  static count = 0;
  constructor(public sides: number) {}
  static make(sides: number): Shape {
    this.count++;
    return new Shape(sides);
  }
  static triangle(): Shape {
    return this.make(3);
  }
  static describe(s: Shape): string {
    return Shape.names(s.sides) + " #" + this.count;
  }
  static names(n: number): string {
    switch (n) {
      case 3:
        return "triangle";
      case 4:
        return "square";
    }
    return n + "-gon";
  }
}
let shapes = "";
for (let i = 0; i < 3; i++) shapes += Shape.describe(i === 0 ? Shape.triangle() : Shape.make(3 + i)) + "; ";
console.log(shapes);
