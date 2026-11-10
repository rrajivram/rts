// Counting loop with arithmetic: 10M iterations.
let acc = 0;
for (let i = 0; i < 10_000_000; i++) {
  acc = (acc + i * 3) % 1_000_003;
}
console.log(acc);
