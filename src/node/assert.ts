// node:assert
export class AssertionError extends Error {
  actual: any;
  expected: any;
  operator: string;
  code = "ERR_ASSERTION";
  constructor(message: string, actual?: any, expected?: any, operator?: string) {
    super(message);
    this.name = "AssertionError";
    this.actual = actual;
    this.expected = expected;
    this.operator = operator ?? "";
  }
}

function fail(message: any, fallback: string, actual?: any, expected?: any, operator?: string): never {
  if (message instanceof Error) throw message;
  throw new AssertionError(message ?? fallback, actual, expected, operator);
}

export function ok(value: any, message?: any): void {
  if (!value) fail(message, "The expression evaluated to a falsy value:\n\n  assert.ok(" + __rts_inspect(value, 2) + ")\n", value, true, "==");
}
export function equal(actual: any, expected: any, message?: any): void {
  if (actual != expected) fail(message, `${__rts_inspect(actual, 2)} == ${__rts_inspect(expected, 2)}`, actual, expected, "==");
}
export function notEqual(actual: any, expected: any, message?: any): void {
  if (actual == expected) fail(message, `${__rts_inspect(actual, 2)} != ${__rts_inspect(expected, 2)}`, actual, expected, "!=");
}
export function strictEqual(actual: any, expected: any, message?: any): void {
  if (!Object.is(actual, expected)) {
    fail(message, `Expected values to be strictly equal:\n\n${__rts_inspect(actual, 2)} !== ${__rts_inspect(expected, 2)}\n`, actual, expected, "strictEqual");
  }
}
export function notStrictEqual(actual: any, expected: any, message?: any): void {
  if (Object.is(actual, expected)) fail(message, `Expected "actual" to be strictly unequal to: ${__rts_inspect(expected, 2)}`, actual, expected, "notStrictEqual");
}
export function deepEqual(actual: any, expected: any, message?: any): void {
  deepStrictEqual(actual, expected, message);
}
export function deepStrictEqual(actual: any, expected: any, message?: any): void {
  if (!__rts_deep_equal(actual, expected)) {
    fail(message, `Expected values to be strictly deep-equal:\n${__rts_inspect(actual, 2)}\n\nshould equal\n\n${__rts_inspect(expected, 2)}`, actual, expected, "deepStrictEqual");
  }
}
export function notDeepStrictEqual(actual: any, expected: any, message?: any): void {
  if (__rts_deep_equal(actual, expected)) fail(message, "Expected values not to be deep-equal", actual, expected, "notDeepStrictEqual");
}
export function throws(f: () => any, expected?: any, message?: any): void {
  try {
    f();
  } catch (e) {
    if (typeof expected === "function" && expected.prototype === undefined && !(e instanceof Error)) return;
    return;
  }
  fail(typeof expected === "string" ? expected : message, "Missing expected exception.", undefined, expected, "throws");
}
export async function rejects(p: any, expected?: any, message?: any): Promise<void> {
  try {
    await (typeof p === "function" ? p() : p);
  } catch (e) {
    return;
  }
  fail(message, "Missing expected rejection.", undefined, expected, "rejects");
}
export function doesNotThrow(f: () => any, message?: any): void {
  try {
    f();
  } catch (e) {
    fail(message, "Got unwanted exception.\nActual message: \"" + (e as Error).message + "\"", e, undefined, "doesNotThrow");
  }
}
export function match(s: string, re: RegExp, message?: any): void {
  if (!re.test(s)) fail(message, `The input did not match the regular expression ${String(re)}. Input:\n\n${__rts_inspect(s, 2)}\n`, s, re, "match");
}

export function assert(value: any, message?: any): void {
  ok(value, message);
}
assert.ok = ok;
assert.equal = equal;
assert.notEqual = notEqual;
assert.strictEqual = strictEqual;
assert.notStrictEqual = notStrictEqual;
assert.deepEqual = deepEqual;
assert.deepStrictEqual = deepStrictEqual;
assert.notDeepStrictEqual = notDeepStrictEqual;
assert.throws = throws;
assert.rejects = rejects;
assert.doesNotThrow = doesNotThrow;
assert.match = match;
assert.strict = assert;
assert.AssertionError = AssertionError;
export const strict = assert;
export default assert;
