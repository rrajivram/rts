abstract class Shape {
  constructor(readonly name: string) {}
  abstract area(): number;
  describe(): string {
    return `${this.name} with area ${this.area().toFixed(2)}`;
  }
}

class Circle extends Shape {
  constructor(private r: number) {
    super("circle");
  }
  area(): number {
    return Math.PI * this.r * this.r;
  }
}

class Rect extends Shape {
  constructor(private w: number, private h: number) {
    super("rect");
  }
  area(): number {
    return this.w * this.h;
  }
  describe(): string {
    return "Rect! " + super.describe();
  }
}

class Square extends Rect {
  constructor(side: number) {
    super(side, side);
  }
}

const shapes: Shape[] = [new Circle(1), new Rect(2, 3), new Square(4)];
for (const s of shapes) {
  console.log(s.describe());
}
let sum = 0;
for (const s of shapes) sum += s.area();
console.log(sum.toFixed(3));
console.log(shapes[2] instanceof Rect, shapes[0] instanceof Rect, shapes[2] instanceof Shape);

class Stack<T> {
  private items: T[] = [];
  push(x: T): void {
    this.items.push(x);
  }
  pop(): T | undefined {
    return this.items.pop();
  }
  peek(): T | undefined {
    return this.items[this.items.length - 1];
  }
  size(): number {
    return this.items.length;
  }
}

const st = new Stack<number>();
st.push(1);
st.push(2);
st.push(3);
console.log(st.pop(), st.size(), st.peek());
const ss = new Stack<string>();
ss.push("a");
console.log(ss.pop(), ss.pop());

class Static {
  static count = 0;
  static make(): Static {
    Static.count++;
    return new Static();
  }
}
Static.make();
Static.make();
console.log(Static.count);

class Node2 {
  next: Node2 | null = null;
  constructor(public value: number) {}
}
let head: Node2 | null = null;
for (let i = 0; i < 5; i++) {
  const n = new Node2(i);
  n.next = head;
  head = n;
}
let cur = head;
const vals: number[] = [];
while (cur !== null) {
  vals.push(cur.value);
  cur = cur.next;
}
console.log(vals.join(","));

class Animal {
  speak(): string { return "..."; }
  greet(): string { return "I say " + this.speak(); }
}
class Dog extends Animal {
  speak(): string { return "woof"; }
}
const a: Animal = new Dog();
console.log(a.greet());
