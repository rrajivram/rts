// Type-level features: mapped, conditional, indexed access, keyof, infer.
interface User {
  id: number;
  name: string;
  email?: string;
}
type Opt<T> = { [K in keyof T]?: T[K] };
type Req<T> = { [K in keyof T]-?: T[K] };
type RO<T> = { readonly [K in keyof T]: T[K] };
type PickT<T, K extends keyof T> = { [P in K]: T[P] };
type IsStr<T> = T extends string ? "yes" : "no";
type ElemOf<T> = T extends (infer U)[] ? U : never;
type Unwrap<T> = T extends Promise<infer V> ? V : T;
type Fn = (a: number, b: string) => boolean;
type Ret<F> = F extends (...args: any[]) => infer R ? R : never;
type NameT = User["name"];
type Keys = keyof User;
type Id = `user-${number}`;

const partial: Opt<User> = { name: "x" };
const full: Req<User> = { id: 1, name: "n", email: "e" };
const ro: RO<User> = { id: 2, name: "r" };
const picked: PickT<User, "id" | "name"> = { id: 3, name: "p" };
const yes: IsStr<string> = "yes";
const no: IsStr<number> = "no";
const elem: ElemOf<string[]> = "elem";
const unwrapped: Unwrap<Promise<number>> = 42;
const ret: Ret<Fn> = true;
const nm: NameT = "just a string";
const key: Keys = "email";
const id: Id = "user-7";
console.log(partial, full, ro, picked, yes, no, elem, unwrapped, ret, nm, key, id);

function getProp<T, K extends keyof T>(obj: T, k: K): T[K] {
  return obj[k];
}
console.log(getProp(full, "name"), getProp({ a: [1, 2] }, "a").length);

type Handlers = { [E in "click" | "key"]: (e: string) => void };
const h: Handlers = { click: (e) => console.log("click", e), key: (e) => console.log("key", e) };
h.click("btn");
h.key("enter");
type Rec = Record<"a" | "b", number>;
const r: Rec = { a: 1, b: 2 };
console.log(r.a + r.b, typeof (partial.name satisfies string | undefined));
