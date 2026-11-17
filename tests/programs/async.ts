// Ordering of promise callbacks, microtasks and timers must match Node.
// (The long timers leave a wide margin over the short delays, so the
// order holds on a loaded machine.)
console.log("sync start");
setTimeout(() => console.log("timeout 0"), 0);
setTimeout(() => console.log("timeout 150"), 450);
setTimeout(() => console.log("timeout 60"), 180);
queueMicrotask(() => console.log("microtask"));
Promise.resolve(1).then((v) => console.log("then", v));

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function add(a: number, b: number): Promise<number> {
  await delay(1);
  return a + b;
}

async function main(): Promise<void> {
  console.log("main start");
  const x = await add(1, 2);
  console.log("x =", x);
  const [p, q] = await Promise.all([add(3, 4), add(5, 6)]);
  console.log("all", p, q);
  try {
    await Promise.reject(new Error("nope"));
  } catch (e) {
    console.log("caught", (e as Error).message);
  }
  const results = await Promise.allSettled([Promise.resolve(1), Promise.reject("bad")]);
  console.log(results);
  const first = await Promise.race([delay(100).then(() => "slow"), delay(30).then(() => "fast")]);
  console.log("race", first);
  const anyOk = await Promise.any([Promise.reject(1), Promise.resolve(2)]);
  console.log("any", anyOk);
  console.log("main end");
}

main().then(() => console.log("main resolved"));
console.log("sync end");

// Interleaving of two async functions.
async function ticker(name: string, n: number): Promise<number> {
  for (let i = 0; i < n; i++) {
    console.log(name, i);
    await null;
  }
  return n;
}
ticker("a", 3);
ticker("b", 2);

// then chaining, catch, finally.
Promise.resolve(10)
  .then((v) => v * 2)
  .then((v) => {
    throw new Error("at " + v);
  })
  .catch((e: Error) => e.message)
  .finally(() => console.log("finally"))
  .then((m) => console.log("chain", m));

// Returning a promise from then, and from an async function.
async function nested(): Promise<string> {
  return Promise.resolve("inner");
}
nested().then((s) => console.log("nested", s));

// Async arrow functions and methods.
class Service {
  constructor(private base: number) {}
  async compute(x: number): Promise<number> {
    await delay(1);
    return this.base * x;
  }
}
const svc = new Service(7);
const go = async () => console.log("service", await svc.compute(6));
go();

// Async generators and for await.
async function* produce(): AsyncGenerator<number> {
  for (let i = 0; i < 3; i++) {
    await delay(1);
    yield i;
  }
}
(async () => {
  let total = 0;
  for await (const v of produce()) {
    console.log("produced", v);
    total += v;
  }
  for await (const v of [Promise.resolve("p1"), "plain"]) console.log("awaited", v);
  console.log("total", total);
})();

const p = new Promise<number>((resolve) => resolve(42));
console.log(p, Promise.reject(new Error("x")).catch(() => 0) instanceof Promise);
setTimeout(() => console.log(p), 600);
let ticks = 0;
const iv = setInterval(() => {
  ticks++;
  if (ticks === 3) {
    clearInterval(iv);
    console.log("interval done");
  }
}, 60);
