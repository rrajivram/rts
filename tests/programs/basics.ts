function fib(n: number): number {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}
console.log("fib(20) =", fib(20));

let total = 0;
for (let i = 0; i < 10; i++) {
  total += i * i;
}
console.log(`total: ${total}`);

const xs: number[] = [3, 1, 2];
xs.push(10);
xs.sort((a, b) => a - b);
console.log(xs, xs.length);

const words = ["apple", "banana", "cherry"];
console.log(words.map(w => w.toUpperCase()).join("-"));
const lens = words.map(w => w.length);
console.log(lens.reduce((a, b) => a + b, 0));

interface Point { x: number; y: number }
function dist(p: Point): number {
  return Math.sqrt(p.x * p.x + p.y * p.y);
}
console.log(dist({ x: 3, y: 4 }));

class Counter {
  count = 0;
  constructor(public name: string) {}
  inc(by: number = 1): this {
    this.count += by;
    return this;
  }
}
const c = new Counter("clicks");
c.inc().inc(5);
console.log(c.name, c.count, c);

function makeAdder(n: number) {
  return (x: number) => x + n;
}
const add5 = makeAdder(5);
console.log(add5(10));

let maybe: string | null = null;
console.log(maybe ?? "default");
maybe = "set";
if (maybe !== null) console.log(maybe.length);
