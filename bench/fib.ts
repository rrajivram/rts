// Recursive calls: fib(30) ≈ 1.6M calls.
function fib(n: number): number {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}
console.log(fib(30));
