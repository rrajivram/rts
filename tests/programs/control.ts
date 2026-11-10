function classify(n: number): string {
  switch (n % 3) {
    case 0:
      return "fizz";
    case 1:
    case 2: {
      const s = "other" + n;
      return s;
    }
    default:
      return "never";
  }
}
console.log([3, 4, 5].map(classify));

outer: for (let i = 0; i < 5; i++) {
  for (let j = 0; j < 5; j++) {
    if (j === 3) continue outer;
    if (i === 3) break outer;
    if (j > i) break;
    console.log(i, j);
  }
}

let k = 0;
do {
  k += 2;
} while (k < 7);
console.log(k);

function risky(n: number): number {
  if (n > 2) throw new RangeError("too big: " + n);
  return n;
}
for (const n of [1, 5]) {
  try {
    console.log("ok", risky(n));
  } catch (e) {
    if (e instanceof RangeError) console.log("caught", e.message, e.name);
  } finally {
    console.log("finally", n);
  }
}

function withFinally(): string {
  try {
    return "from try";
  } finally {
    console.log("cleanup ran");
  }
}
console.log(withFinally());

class ValidationError extends Error {
  constructor(public field: string) {
    super("invalid " + field);
    this.name = "ValidationError";
  }
}
try {
  throw new ValidationError("email");
} catch (e) {
  if (e instanceof ValidationError) console.log(e.field, e.message, String(e));
}

try {
  const o: any = null;
  console.log(o.x);
} catch (e) {
  console.log("TypeError?", e instanceof TypeError);
}

enum Color { Red, Green = 5, Blue }
enum Dir { Up = "UP", Down = "DOWN" }
console.log(Color.Red, Color.Green, Color.Blue, Dir.Up);
function colorName(c: Color): string {
  return c === Color.Green ? "green" : "not green";
}
console.log(colorName(Color.Green), colorName(Color.Blue));

let total = 0;
for (let i = 0; i < 100; i++) {
  if (i % 7 === 0) continue;
  total += i;
}
console.log(total);
const obj = { a: 1, b: 2, c: 3 };
const keys: string[] = [];
for (const key in obj) keys.push(key);
console.log(keys);
let x = 10;
x -= 3; x *= 2; x /= 7; x **= 3; x %= 5;
console.log(x, 7 & 3, 7 | 8, 5 ^ 1, ~5, 1 << 4, -16 >> 2, -16 >>> 28);
let cnt = 0;
const r1 = cnt++ + ++cnt;
console.log(r1, cnt);
