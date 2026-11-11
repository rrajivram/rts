import square, { PI2, counter, bump, Vec2, Color, type Point, type Pair } from "./modules/math.ts";
import * as str from "./modules/strings.ts";
import { sq, strings, Vec2 as V } from "./modules/index.ts";
import def from "./modules/default_expr.ts";

console.log("main start");
console.log(square(7), sq(3), PI2, Color.Green);
console.log(counter);
bump();
bump();
console.log(counter); // live binding
console.log(str.greet("ts"), str.shout("hi"), str.answer, strings.answer);
console.log(counter);
const p: Point = new Vec2(3, 4);
const pair: Pair = [1, 2];
console.log(p.x, new V(6, 8).len(), pair);
console.log(def.name, def.items.length);
const ns = str;
console.log(Object.keys(ns).join(","));
