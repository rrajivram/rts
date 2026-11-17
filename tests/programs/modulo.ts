// % in the interpreter and compiled code: integers, signs, -0, fractions, NaN, Infinity.
const xs = [7, -7, 0, -0, 5.5, 1e20, 2**53, NaN, Infinity, 3, 9007199254740993];
const ys = [3, -3, 0, -0, 2.5, 7, Infinity, NaN, 1, 1e300];
function m(a: number, b: number): number { return a % b; }
const out: string[] = [];
for (const a of xs) for (const b of ys) { const r = m(a, b); out.push(Object.is(r, -0) ? "-0" : String(r)); }
console.log(out.join(" "));
const za = [-0];
const z = -0;
let w = 0; w = -w;
console.log(Object.is(za[0], -0), Object.is(z, -0), Object.is(w, -0), Object.is(z % 3, -0), Object.is(w % 3, -0));
