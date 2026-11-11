function* count(n: number): Generator<number, string, unknown> {
  for (let i = 0; i < n; i++) {
    yield i;
  }
  return "done";
}

const g = count(3);
console.log(g.next(), g.next(), g.next(), g.next(), g.next());

for (const x of count(4)) console.log("x", x);
console.log([...count(5)], Array.from(count(3)));

function* fib(): Generator<number> {
  let [a, b] = [0, 1];
  while (true) {
    yield a;
    [a, b] = [b, a + b];
  }
}
const firstTen: number[] = [];
for (const f of fib()) {
  if (f > 50) break;
  firstTen.push(f);
}
console.log(firstTen);

// Two-way communication.
function* accumulator() {
  let total = 0;
  while (true) {
    const x: number = yield total;
    total += x;
  }
}
const acc = accumulator();
acc.next();
acc.next(5);
console.log(acc.next(10).value);

// finally runs on early exit; return() and throw().
function* guarded() {
  try {
    yield 1;
    yield 2;
    yield 3;
  } finally {
    console.log("cleanup");
  }
}
for (const v of guarded()) {
  console.log("got", v);
  if (v === 2) break;
}
const gr = guarded();
console.log(gr.next(), gr.return(99), gr.next());
function* catcher() {
  while (true) {
    try {
      yield "waiting";
    } catch (e) {
      console.log("caught", e);
    }
  }
}
const c = catcher();
c.next();
console.log(c.throw("boom"));

// yield* delegation.
function* inner() {
  yield "a";
  yield "b";
  return "inner-result";
}
function* outer() {
  const r = yield* inner();
  console.log("inner returned", r);
  yield* [1, 2];
  yield* "hi";
}
console.log([...outer()]);

// Iterables.
class Range implements Iterable<number> {
  constructor(private lo: number, private hi: number) {}
  *[Symbol.iterator](): Iterator<number> {
    for (let i = this.lo; i < this.hi; i++) yield i;
  }
}
let sum = 0;
for (const n of new Range(1, 5)) sum += n;
console.log(sum, [...new Range(0, 3)]);

const manual = {
  i: 0,
  [Symbol.iterator]() {
    return this;
  },
  next(): IteratorResult<number> {
    this.i++;
    return this.i <= 3 ? { value: this.i * 10, done: false } : { value: undefined, done: true };
  },
};
for (const m of manual) console.log("m", m);

function* entries<T>(xs: T[]): Generator<[number, T]> {
  let i = 0;
  for (const x of xs) yield [i++, x];
}
for (const [i, s] of entries(["p", "q"])) console.log(i, s);
console.log(typeof g, g);
