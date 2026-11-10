// Higher-order functions: map/filter/reduce with arrow callbacks over 1M items (x5).
const xs: number[] = [];
for (let i = 0; i < 1_000_000; i++) xs.push(i);
let r = 0;
for (let k = 0; k < 5; k++) {
  r += xs.map(x => x * 2).filter(x => x % 3 === 0).reduce((a, b) => a + b, 0);
}
console.log(r);
