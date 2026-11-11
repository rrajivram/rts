import { bump } from "./math.ts";
export const greet = (name: string): string => `hello ${name}`;
export function shout(s: string): string {
  bump();
  return s.toUpperCase();
}
const hidden = 42;
export { hidden as answer };
console.log("strings loaded");
