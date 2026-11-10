// `any` interop: values flow through boxed paths and are checked when they re-enter typed code.
const anyVal: any = { name: "box", items: [1, 2, 3] };
console.log(anyVal.name, anyVal.items.length, anyVal.missing);
anyVal.extra = 42;
console.log(anyVal);
function addAny(a: any, b: any): any {
  return a + b;
}
console.log(addAny(1, 2), addAny("a", 1), addAny([1], 2));
const n: number = addAny(2, 3);
console.log(n * 2);
const dict: Record<string, number> = {};
dict["a"] = 1;
dict.b = 2;
for (const k of Object.keys(dict)) console.log(k, dict[k]);
console.log(typeof 1, typeof "s", typeof {}, typeof null, typeof undefined, typeof (() => 1));
const u: unknown = "text";
if (typeof u === "string") console.log(u.toUpperCase());
const mixed: (number | string)[] = [1, "two", 3];
console.log(mixed.map(m => typeof m === "number" ? m * 2 : m + "!"));
const optional: { a?: { b?: number } } = { a: {} };
console.log(optional.a?.b, optional.a?.b ?? "none");
const fn: ((x: number) => number) | undefined = undefined;
console.log(fn?.(3));
