export const PI2 = 6.28;
export let counter = 0;
export function bump(): number {
  counter++;
  return counter;
}
export default function square(x: number): number {
  return x * x;
}
export interface Point {
  x: number;
  y: number;
}
export type Pair = [number, number];
export class Vec2 implements Point {
  constructor(public x: number, public y: number) {}
  len(): number {
    return Math.sqrt(this.x * this.x + this.y * this.y);
  }
}
export enum Color {
  Red,
  Green,
}
console.log("math loaded");
