// BigInts: unboxed i128 in typed code, boxed when untyped.

function factorial(n: bigint): bigint {
  let r = 1n;
  for (let i = 2n; i <= n; i++) {
    r *= i;
  }
  return r;
}

function fib(n: number): bigint {
  let a = 0n;
  let b = 1n;
  for (let i = 0; i < n; i++) {
    const t = a + b;
    a = b;
    b = t;
  }
  return a;
}

function gcd(a: bigint, b: bigint): bigint {
  while (b !== 0n) {
    const t = a % b;
    a = b;
    b = t;
  }
  return a < 0n ? -a : a;
}

function modpow(base: bigint, exp: bigint, m: bigint): bigint {
  let result = 1n;
  base %= m;
  while (exp > 0n) {
    if (exp & 1n) result = (result * base) % m;
    base = (base * base) % m;
    exp >>= 1n;
  }
  return result;
}

console.log(factorial(20n));
console.log(factorial(30n));
console.log(fib(150));
console.log(gcd(1071n, -462n));
console.log(modpow(4n, 13n, 497n));
console.log(2n ** 100n, -7n / 2n, -7n % 3n, 5n << 3n, -9n >> 1n, ~5n, 6n & 3n, 6n | 3n, 6n ^ 3n);

// Literals in other radixes, conversions, printing.
const big: bigint = 0xffff_ffff_ffff_ffffn;
console.log(big, 0b1010n, 0o17n, big + 1n);
console.log(BigInt(42), BigInt("12345678901234567890"), BigInt(true), BigInt(" 0x1f "));
console.log(Number(123n), String(-5n), (255n).toString(16), (-255n).toString(2), `${10n ** 20n}`);
console.log("total: " + 3n, typeof 3n, [1n, 2n, 3n], { n: 7n });

// Comparisons, equality, truthiness.
console.log(1n < 2n, 3n >= 3n, 2n === 2n, 2n !== 3n, 1n < 1.5, 2n > 1.5, 1n == 1, 0n == false);
console.log(0n ? "yes" : "no", 5n ? "yes" : "no", !0n, 0n || "fallback", 3n && "and");

// Through `any`, objects, arrays, closures, maps.
const xs: bigint[] = [5n, 3n, 9n];
let sum = 0n;
for (const x of xs) sum += x;
console.log(sum, xs.map((x) => x * 2n), xs.indexOf(3n), xs.includes(9n));
const a: any = 10n;
const b: any = 4n;
console.log(a + b, a * b, a - b, a / b, a % b, a < b, a > 3, typeof a);
let counter: any = 1n;
counter++;
console.log(counter);
class Account {
  balance: bigint = 0n;
  deposit(x: bigint): void {
    this.balance += x;
  }
}
const acct = new Account();
acct.deposit(10n ** 18n);
acct.deposit(1n);
console.log(acct.balance);
const m = new Map<bigint, string>();
m.set(1n, "one");
console.log(m.get(1n), m.has(2n));
let captured = 0n;
const bump = () => {
  captured += 2n;
};
bump();
bump();
console.log(captured);

// Errors.
function attempt(f: () => unknown): void {
  try {
    console.log(f());
  } catch (e: any) {
    console.log(e.name + ": " + e.message);
  }
}
attempt(() => 1n / 0n);
attempt(() => a + 1);
attempt(() => BigInt(1.5));
attempt(() => JSON.stringify({ n: 1n }));
attempt(() => {
  const u: any = 1n;
  return +u;
});
