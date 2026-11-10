function counter() {
  let n = 0;
  return {
    inc: () => ++n,
    get: () => n,
  };
}
const c1 = counter();
c1.inc();
c1.inc();
const c2 = counter();
c2.inc();
console.log(c1.get(), c2.get());

const fns: (() => number)[] = [];
for (let i = 0; i < 3; i++) {
  fns.push(() => i * 10);
}
console.log(fns.map(f => f()));

function compose<A, B, C>(f: (b: B) => C, g: (a: A) => B): (a: A) => C {
  return (x: A) => f(g(x));
}
const inc = (x: number) => x + 1;
const dbl = (x: number) => x * 2;
console.log(compose(inc, dbl)(5), compose(dbl, inc)(5));

function memo(f: (n: number) => number): (n: number) => number {
  const cache = new Map<number, number>();
  return (n: number) => {
    const hit = cache.get(n);
    if (hit !== undefined) return hit;
    const v = f(n);
    cache.set(n, v);
    return v;
  };
}
let calls = 0;
const slowSquare = (n: number) => {
  calls++;
  return n * n;
};
const fast = memo(slowSquare);
console.log(fast(4), fast(4), fast(5), calls);

function outer() {
  let x = 1;
  function inner() {
    function innermost() {
      x += 10;
      return x;
    }
    return innermost();
  }
  inner();
  return inner();
}
console.log(outer());

class Btn {
  clicks = 0;
  handler() {
    return () => {
      this.clicks++;
      return this.clicks;
    };
  }
}
const b = new Btn();
const h = b.handler();
h();
h();
console.log(b.clicks);

const adders = [1, 2, 3].map(n => (x: number) => x + n);
console.log(adders.map(f => f(100)));
