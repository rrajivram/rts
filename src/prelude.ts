// Built-in classes written in TypeScript, compiled with every program.

class Error {
  name: string;
  message: string;
  stack: string;
  cause?: unknown;
  constructor(message?: string, options?: { cause?: unknown }) {
    this.name = "Error";
    this.message = message === undefined ? "" : message;
    this.stack = "";
    if (options !== undefined) this.cause = options.cause;
  }
  toString(): string {
    return this.message === "" ? this.name : this.name + ": " + this.message;
  }
}

class TypeError extends Error {
  constructor(message?: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = "TypeError";
  }
}

class RangeError extends Error {
  constructor(message?: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = "RangeError";
  }
}

class SyntaxError extends Error {
  constructor(message?: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = "SyntaxError";
  }
}

class ReferenceError extends Error {
  constructor(message?: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = "ReferenceError";
  }
}

class EvalError extends Error {
  constructor(message?: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = "EvalError";
  }
}

class URIError extends Error {
  constructor(message?: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = "URIError";
  }
}

class AggregateError extends Error {
  errors: any[];
  constructor(errors: any[], message?: string) {
    super(message);
    this.name = "AggregateError";
    this.errors = errors;
  }
}

