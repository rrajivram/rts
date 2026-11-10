class Temperature {
  private celsius = 0;
  get fahrenheit(): number {
    return this.celsius * 9 / 5 + 32;
  }
  set fahrenheit(f: number) {
    this.celsius = (f - 32) * 5 / 9;
  }
  get c(): number { return this.celsius; }
}
const t = new Temperature();
t.fahrenheit = 212;
console.log(t.c, t.fahrenheit);
t.fahrenheit += 18;
console.log(t.c.toFixed(1));

class Stack<T> {
  private items: T[] = [];
  push(x: T) { this.items.push(x); }
  get size(): number { return this.items.length; }
  get top(): T | undefined { return this.items[this.items.length - 1]; }
}
const s = new Stack<string>();
s.push("a");
s.push("b");
console.log(s.size, s.top);

class Base {
  get kind(): string { return "base"; }
  describe(): string { return "I am " + this.kind; }
}
class Derived extends Base {
  get kind(): string { return "derived"; }
}
console.log(new Base().describe(), new Derived().describe());
const anyT: any = t;
console.log(anyT.fahrenheit);
anyT.fahrenheit = 32;
console.log(t.c);
interface HasSize { readonly size: number }
const hs: HasSize = s;
console.log(hs.size);
