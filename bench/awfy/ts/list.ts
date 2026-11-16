// Typed port of the are-we-fast-yet benchmark of the same name (MIT license; see ../LICENSE.md).
import { Benchmark } from './benchmark.ts';

class Element {
  val: number;
  next: Element | null;

  constructor(v: number) {
    this.val = v;
    this.next = null;
  }

  length(): number {
    if (this.next === null) {
      return 1;
    }
    return 1 + this.next.length();
  }
}

class List extends Benchmark {
  benchmark(): number {
    const result = this.tail(this.makeList(15), this.makeList(10), this.makeList(6));
    return (result as Element).length();
  }

  makeList(length: number): Element | null {
    if (length === 0) {
      return null;
    }
    const e = new Element(length);
    e.next = this.makeList(length - 1);
    return e;
  }

  isShorterThan(x: Element | null, y: Element | null): boolean {
    let xTail = x;
    let yTail = y;
    while (yTail !== null) {
      if (xTail === null) { return true; }
      xTail = xTail.next;
      yTail = yTail.next;
    }
    return false;
  }

  tail(x: Element | null, y: Element | null, z: Element | null): Element | null {
    if (this.isShorterThan(y, x)) {
      return this.tail(
        this.tail((x as Element).next, y, z),
        this.tail((y as Element).next, z, x),
        this.tail((z as Element).next, x, y),
      );
    }
    return z;
  }

  verifyResult(result: number): boolean {
    return 10 === result;
  }
}

export const newInstance = (): Benchmark => new List();
