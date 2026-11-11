// node:timers/promises
export function setTimeout<T>(ms?: number, value?: T): Promise<T> {
  return new Promise((resolve) => globalThis.setTimeout(() => resolve(value as T), ms ?? 0));
}
export function setImmediate<T>(value?: T): Promise<T> {
  return new Promise((resolve) => globalThis.setImmediate(() => resolve(value as T)));
}
export default { setTimeout, setImmediate };
