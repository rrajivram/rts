// Built-in classes written in TypeScript, compiled with every program.

class Error {
  name: string;
  message: string;
  stack: string;
  constructor(message?: string) {
    this.name = "Error";
    this.message = message === undefined ? "" : message;
    this.stack = "";
  }
  toString(): string {
    return this.message === "" ? this.name : this.name + ": " + this.message;
  }
}

class TypeError extends Error {
  constructor(message?: string) {
    super(message);
    this.name = "TypeError";
  }
}

class RangeError extends Error {
  constructor(message?: string) {
    super(message);
    this.name = "RangeError";
  }
}

class SyntaxError extends Error {
  constructor(message?: string) {
    super(message);
    this.name = "SyntaxError";
  }
}
