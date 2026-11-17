// Native code with closures: captured variables in cells, closures made
// and called by compiled code, closures entered from the interpreter.

class Counter {
  n = 0;
  step = 1;
  // `this` captured by an arrow function.
  adder(): () => number {
    return () => {
      this.n += this.step;
      return this.n;
    };
  }
  addAll(xs: number[]): number {
    xs.forEach((x) => {
      this.n += x;
    });
    return this.n;
  }
}

function makeCounter(start: number): [() => number, (d: number) => void] {
  // A captured parameter, changed by the closures and read back.
  const inc = (): number => ++start;
  const add = (d: number): void => {
    start += d;
  };
  return [inc, add];
}

function sumWith(xs: number[], scale: number): number {
  let total = 0;
  xs.forEach((x) => {
    total += x * scale;
  });
  return total;
}

function perIteration(): number[] {
  // Each iteration's closure sees its own `i`.
  const fs: (() => number)[] = [];
  for (let i = 0; i < 5; i++) {
    fs.push(() => i * 10);
  }
  const out: number[] = [];
  for (let j = 0; j < fs.length; j++) out.push(fs[j]());
  return out;
}

function blockScoped(): string {
  const fs: (() => string)[] = [];
  for (let i = 0; i < 3; i++) {
    const label = "item" + i;
    let hits = 0;
    fs.push(() => label + ":" + ++hits);
  }
  let s = "";
  for (let j = 0; j < fs.length; j++) s = s + fs[j]() + "," + fs[j]() + " ";
  return s;
}

function nested(a: number): () => () => number {
  // Cells captured through two levels.
  let b = a * 2;
  return () => {
    b += 1;
    return () => a + b;
  };
}

function recursiveInner(n: number): number {
  let calls = 0;
  function fib(k: number): number {
    calls++;
    return k < 2 ? k : fib(k - 1) + fib(k - 2);
  }
  return fib(n) * 1000 + calls;
}

function kinds(): string {
  let num = 1;
  let flag = false;
  let str = "s";
  let obj: { v: number } | null = null;
  let arr: number[] = [];
  const touch = (): void => {
    num += 0.5;
    flag = !flag;
    str = str + "!";
    obj = { v: num };
    arr.push(num);
  };
  touch();
  touch();
  return [num, flag, str, obj === null ? -1 : (obj as { v: number }).v, arr.join("/")].join(" ");
}

class Holder {
  constructor(public v: number) {}
}

function compiledKinds(): string {
  let num = 1;
  let flag = false;
  let str = "s";
  let obj: Holder | null = null;
  let arr: number[] = [];
  const touch = (): void => {
    num += 0.5;
    flag = !flag;
    str = str + "!";
    obj = new Holder(num);
    arr.push(num);
  };
  touch();
  touch();
  arr = [num];
  touch();
  const o = obj as Holder | null;
  return [num, flag, str, o === null ? -1 : o.v, arr.join("/")].join(" ");
}

function outlives(): () => number {
  let hidden = 41;
  return () => ++hidden;
}

function throwsInside(xs: number[]): number {
  let seen = 0;
  try {
    xs.forEach((x) => {
      if (x < 0) throw new Error("negative " + x + " after " + seen);
      seen++;
    });
  } catch (e) {
    console.log((e as Error).message);
  }
  return seen;
}

function filterMap(xs: number[], min: number): number[] {
  return xs.filter((x) => x >= min).map((x) => x + min);
}

const c = new Counter();
const add1 = c.adder();
let last = 0;
for (let i = 0; i < 1000; i++) last = add1();
console.log(last, c.addAll([1, 2, 3]));

const [inc, add] = makeCounter(10);
inc();
add(5);
console.log(inc());

let total = 0;
for (let i = 0; i < 200; i++) total += sumWith([1, 2, 3], i);
console.log(total);
console.log(perIteration().join(","), blockScoped());

const g = nested(5);
const h1 = g();
const h2 = g();
console.log(h1(), h2());
console.log(recursiveInner(15));
console.log(kinds(), compiledKinds());
const o = outlives();
o();
console.log(o(), o());
console.log(throwsInside([1, 2, 3]), throwsInside([1, -2, 3]));
console.log(filterMap([1, 5, 2, 8, 3], 3).join(" "));

// Local functions that call themselves (captured in their own cells).
function outer(n: number): number {
  function down(k: number): number {
    return k <= 0 ? 0 : 1 + down(k - 1);
  }
  return down(n);
}
function outerBlock(n: number): number {
  if (n > 0) {
    function down2(k: number): number {
      return k <= 0 ? 0 : 1 + down2(k - 1);
    }
    return down2(n);
  }
  return -1;
}
function inLoop(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    function rec(k: number): number { return k <= 0 ? i : rec(k - 1); }
    s += rec(3);
  }
  return s;
}
let selfRef = 0;
for (let i = 0; i < 200; i++) selfRef += outer(10) + outerBlock(5) + inLoop(4);
console.log(selfRef);
