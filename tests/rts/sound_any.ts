// rts checks values coming out of `any` when they enter typed (unboxed) code.
// Node would silently keep the string in a `number` variable.
function addAny(a: any, b: any): any {
  return a + b;
}
try {
  const bad: number = addAny("x", "y");
  console.log("unreachable", bad);
} catch (e) {
  console.log("caught:", (e as Error).message);
}
const nums: number[] = [1, 2, 3];
const alias: any[] = nums;
alias.push("four");
try {
  let s = 0;
  for (const n of nums) s += n;
  console.log(s);
} catch (e) {
  console.log("caught:", (e as Error).message);
}
