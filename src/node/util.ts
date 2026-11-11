// node:util
export function format(...args: any[]): string {
  return __rts_format(...args);
}
export function inspect(value: any, options?: any): string {
  return __rts_inspect(value, options?.depth);
}
export function promisify(f: any): (...args: any[]) => Promise<any> {
  return (...args: any[]) =>
    new Promise((resolve, reject) => {
      f(...args, (err: any, value: any) => (err ? reject(err) : resolve(value)));
    });
}
export function isDeepStrictEqual(a: any, b: any): boolean {
  return __rts_deep_equal(a, b);
}
export function inherits(ctor: any, superCtor: any): void {}
export function deprecate<T>(fn: T, msg: string): T {
  return fn;
}
export const types = {
  isPromise: (v: any): boolean => v instanceof Promise,
  isRegExp: (v: any): boolean => v instanceof RegExp,
  isDate: (v: any): boolean => v instanceof Date,
};
export default { format, inspect, promisify, isDeepStrictEqual, inherits, deprecate, types };
