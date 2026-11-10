type Shape =
  | { kind: "circle"; radius: number }
  | { kind: "square"; side: number }
  | { kind: "rect"; w: number; h: number };

function area(s: Shape): number {
  switch (s.kind) {
    case "circle":
      return Math.PI * s.radius ** 2;
    case "square":
      return s.side * s.side;
    case "rect":
      return s.w * s.h;
  }
}
const shapes: Shape[] = [
  { kind: "circle", radius: 2 },
  { kind: "square", side: 3 },
  { kind: "rect", w: 2, h: 5 },
];
console.log(shapes.map(area).map(a => Math.round(a)));

function describe(x: string | number | boolean | null | undefined): string {
  if (x === null) return "null";
  if (x === undefined) return "undefined";
  if (typeof x === "string") return "string of length " + x.length;
  if (typeof x === "number") return "number " + x.toFixed(1);
  return "boolean " + (x ? "yes" : "no");
}
console.log(describe("hello"), describe(42), describe(true), describe(null), describe(undefined));

function len(x: string | string[]): number {
  if (Array.isArray(x)) {
    return x.length * 100;
  }
  return x.length;
}
console.log(len("abc"), len(["a", "b"]));

class Fish { swim() { return "swims"; } }
class Bird { fly() { return "flies"; } }
function move(a: Fish | Bird): string {
  if (a instanceof Fish) return a.swim();
  return a.fly();
}
console.log(move(new Fish()), move(new Bird()));

interface Tree { value: number; left: Tree | null; right: Tree | null }
function sumTree(t: Tree | null): number {
  if (!t) return 0;
  return t.value + sumTree(t.left) + sumTree(t.right);
}
const tree: Tree = {
  value: 1,
  left: { value: 2, left: null, right: null },
  right: { value: 3, left: { value: 4, left: null, right: null }, right: null },
};
console.log(sumTree(tree));

function first(xs: number[]): number | undefined {
  return xs.length > 0 ? xs[0] : undefined;
}
const f = first([7, 8]);
if (f !== undefined) {
  console.log(f + 1);
}
const g = first([]) ?? -1;
console.log(g);

let u: number | string = 5;
u = "now a string";
console.log(u.toUpperCase());
