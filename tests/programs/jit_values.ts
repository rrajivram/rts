// Native code with values of any type: generics, strings, calls back into
// the interpreter, globals, exceptions crossing compiled frames.

interface Hashable {
  hash(): number;
}

class Key implements Hashable {
  constructor(public k: number) {}
  hash(): number {
    return this.k * 31;
  }
}

class Box<T> {
  items: (T | null)[];
  n: number;
  constructor(size?: number) {
    this.items = new Array<T | null>(size === undefined ? 4 : size).fill(null);
    this.n = 0;
  }
  add(x: T): void {
    if (this.n >= this.items.length) {
      const bigger = new Array<T | null>(this.items.length * 2).fill(null);
      for (let i = 0; i < this.n; i++) bigger[i] = this.items[i];
      this.items = bigger;
    }
    this.items[this.n] = x;
    this.n += 1;
  }
  at(i: number): T | null {
    return i < this.n ? this.items[i] : null;
  }
  forEach(fn: (x: T) => void): void {
    for (let i = 0; i < this.n; i++) fn(this.items[i] as T);
  }
  find(fn: (x: T) => boolean): T | null {
    for (let i = 0; i < this.n; i++) {
      const x = this.items[i] as T;
      if (fn(x)) return x;
    }
    return null;
  }
  contains(x: T): boolean {
    for (let i = 0; i < this.n; i++) if (this.items[i] === x) return true;
    return false;
  }
}

function hashAll(keys: Hashable[]): number {
  let h = 0;
  for (let i = 0; i < keys.length; i++) h = (h + keys[i].hash()) | 0;
  return h;
}

// Values of mixed kinds through one generic container.
const b = new Box<number | string | boolean | null | undefined | Key>();
b.add(1);
b.add("two");
b.add(true);
b.add(null);
b.add(undefined);
const k = new Key(7);
b.add(k);
b.add(NaN);
b.add(0);
b.add("");
console.log(b.n, b.at(1), b.at(5) === k, b.at(42));
console.log(b.contains("two"), b.contains("tw" + "o"), b.contains(k), b.contains(new Key(7)), b.contains(NaN), b.contains(undefined));
const kinds: string[] = [];
b.forEach((x) => kinds.push(typeof x));
console.log(kinds.join(","));
let truthy = 0;
b.forEach((x) => {
  if (x) truthy++;
});
console.log("truthy", truthy);
console.log(b.find((x) => typeof x === "string"), b.find((x) => x === 99));
console.log(hashAll([new Key(1), new Key(2), k]));

// Strings in compiled code.
function joinWords(words: string[], sep: string): string {
  let s = "";
  for (let i = 0; i < words.length; i++) {
    if (i > 0) s = s + sep;
    s = s + words[i];
  }
  return s;
}
function longest(words: string[]): string {
  let best = "";
  for (let i = 0; i < words.length; i++) if (words[i].length > best.length || (words[i].length === best.length && words[i] < best)) best = words[i];
  return best;
}
console.log(joinWords(["a", "bb", "ccc"], "-"), longest(["pear", "fig", "plum", "kiwi"]), "x".length);

// Globals read and written by compiled code, and changed by callees.
let counter = 0;
let label: string | null = null;
function bump(by: number): number {
  counter += by;
  return counter;
}
function relabel(s: string): void {
  label = s;
}
function useGlobals(): string {
  bump(2);
  relabel("set " + counter);
  bump(3);
  return label + " now " + counter;
}
console.log(useGlobals(), counter, label);

// Plain objects: property reads and writes, getters and setters.
const obj = {
  x: 3,
  name: "o",
  get twice() {
    return this.x * 2;
  },
  set half(v: number) {
    this.x = v / 2;
  },
};
function props(o: typeof obj): number {
  o.x = o.x + 1;
  o.half = 10;
  return o.twice + o.x;
}
console.log(props(obj), obj.x, obj.name);

// Exceptions thrown from code compiled code calls, caught outside it.
function mayThrow(n: number): number {
  if (n > 2) throw new RangeError("too big: " + n);
  return n * 10;
}
function sumUpTo(n: number, f: (n: number) => number): number {
  let s = 0;
  for (let i = 0; i <= n; i++) s += f(i);
  return s;
}
try {
  console.log(sumUpTo(2, mayThrow));
  console.log(sumUpTo(5, mayThrow));
} catch (e) {
  console.log("caught", (e as Error).message);
}

// Optional parameters and calls that omit them.
function opt(a: number, b?: number): number {
  return b === undefined ? a : a + b;
}
function callsOpt(): number {
  return opt(1) + opt(1, 2);
}
console.log(callsOpt());

// Equality across kinds.
function eqs(a: unknown, b: unknown): string {
  return [a === b, a == b, a !== b].join(" ");
}
console.log(eqs(1, 1), eqs(1, "1"), eqs(null, undefined), eqs(0, -0), eqs("a", "a"), eqs(k, k));

// Recursion through the interpreter and back.
function depth(n: number, f: (n: number) => number): number {
  return n === 0 ? 0 : 1 + f(n - 1);
}
function viaClosure(n: number): number {
  return depth(n, (m) => viaClosure(m));
}
console.log(viaClosure(200));
