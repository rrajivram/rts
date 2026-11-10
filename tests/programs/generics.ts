interface Box<T> { value: T; map<U>(f: (t: T) => U): Box<U> }
function box<T>(value: T): Box<T> {
  return { value, map: <U>(f: (t: T) => U) => box(f(value)) };
}
console.log(box(21).map(x => x * 2).map(x => "v=" + x).value);
type Pair<A, B> = [A, B];
const p: Pair<string, number> = ["a", 1];
const swap = <A, B>([a, b]: Pair<A, B>): Pair<B, A> => [b, a];
console.log(swap(p));
function sumAll(...nums: number[]): number { let s = 0; for (const n of nums) s += n; return s; }
console.log(sumAll(), sumAll(1, 2, 3), sumAll(...[4, 5]));
const objs = [{ name: "x", age: 3 }, { name: "y", age: 1 }];
const byAge = objs.slice().sort((a, b) => a.age - b.age).map(({ name }) => name);
console.log(byAge);
label: { console.log("in block"); break label; }
console.log(`nested ${`inner ${1 + 1}`}`);
const fnExpr = function named(n: number): number { return n <= 1 ? 1 : n * named(n - 1); };
console.log(fnExpr(5));
