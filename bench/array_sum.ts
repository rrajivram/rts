// Fill a 1M-element number[] with push, then sum it with indexing (x10).
const a: number[] = [];
for (let i = 0; i < 1_000_000; i++) a.push(i % 1000);
let total = 0;
for (let r = 0; r < 10; r++) {
  for (let i = 0; i < a.length; i++) total += a[i];
}
console.log(total);
