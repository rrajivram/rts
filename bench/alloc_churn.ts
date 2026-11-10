// Allocation churn: 2M short-lived objects and arrays. With refcounting each
// one is freed the moment it dies, so memory stays flat without a collector.
class Vec3 {
  x: number; y: number; z: number;
  constructor(x: number, y: number, z: number) { this.x = x; this.y = y; this.z = z; }
  add(o: Vec3): Vec3 { return new Vec3(this.x + o.x, this.y + o.y, this.z + o.z); }
}
let acc = new Vec3(0, 0, 0);
let total = 0;
for (let i = 0; i < 2_000_000; i++) {
  const v = new Vec3(i, i * 2, i * 3);
  acc = acc.add(v);
  const tmp = [i, i + 1, i + 2];
  total += tmp.length;
}
console.log(acc.x, acc.y, acc.z, total);
