// A server-style workload: many short requests, each allocating temporary
// objects that die when it finishes, plus a small long-lived cache.
// Reports a checksum (must match Node) and per-request latency percentiles.
//
//   rts bench/workloads/requests.ts [requests]

class Item {
  id: number;
  price: number;
  qty: number;
  constructor(id: number, price: number, qty: number) {
    this.id = id;
    this.price = price;
    this.qty = qty;
  }
  total(): number {
    return this.price * this.qty;
  }
}

class Point {
  x: number;
  y: number;
  constructor(x: number, y: number) {
    this.x = x;
    this.y = y;
  }
  plus(o: Point): Point {
    return new Point(this.x + o.x, this.y + o.y);
  }
  scale(k: number): Point {
    return new Point(this.x * k, this.y * k);
  }
  dist(o: Point): number {
    const dx = this.x - o.x;
    const dy = this.y - o.y;
    return Math.sqrt(dx * dx + dy * dy);
  }
}

class Order {
  items: Item[] = [];
  customer: number;
  constructor(customer: number) {
    this.customer = customer;
  }
  add(item: Item): void {
    this.items.push(item);
  }
  sum(): number {
    let s = 0;
    for (const it of this.items) {
      s += it.total();
    }
    return s;
  }
}

class Cache {
  keys: number[] = [];
  values: string[] = [];
  capacity: number;
  constructor(capacity: number) {
    this.capacity = capacity;
  }
  get(k: number): string | null {
    for (let i = 0; i < this.keys.length; i++) {
      if (this.keys[i] === k) {
        return this.values[i];
      }
    }
    return null;
  }
  put(k: number, v: string): void {
    if (this.keys.length >= this.capacity) {
      this.keys.shift();
      this.values.shift();
    }
    this.keys.push(k);
    this.values.push(v);
  }
}

let seed = 42;
function rand(n: number): number {
  seed = (seed * 1103515245 + 12345) % 2147483648;
  return seed % n;
}

/// Parse "id:price:qty,id:price:qty,…" into an order.
function parse(customer: number, body: string): Order {
  const order = new Order(customer);
  for (const part of body.split(',')) {
    const f = part.split(':');
    order.add(new Item(parseInt(f[0], 10), parseInt(f[1], 10), parseInt(f[2], 10)));
  }
  return order;
}

/// Route length through the order's delivery points (fresh points per step).
function route(n: number): number {
  let at = new Point(0, 0);
  let length = 0;
  for (let i = 0; i < n; i++) {
    const step = new Point(rand(100) - 50, rand(100) - 50).scale(0.5);
    const next = at.plus(step);
    length += at.dist(next);
    at = next;
  }
  return Math.round(length);
}

function render(order: Order, length: number): string {
  const lines: string[] = [];
  for (const it of order.items) {
    lines.push(`${it.id}x${it.qty}=${it.total()}`);
  }
  return `customer ${order.customer}: ${lines.join(' ')} total ${order.sum()} route ${length}`;
}

function handle(i: number, cache: Cache): number {
  const customer = rand(50);
  const cached = cache.get(customer);
  if (cached !== null && i % 3 === 0) {
    return cached.length;
  }
  const parts: string[] = [];
  const n = 5 + rand(20);
  for (let k = 0; k < n; k++) {
    parts.push(`${rand(1000)}:${1 + rand(99)}:${1 + rand(5)}`);
  }
  const order = parse(customer, parts.join(','));
  const response = render(order, route(20 + rand(30)));
  cache.put(customer, response);
  return response.length;
}

const requests = process.argv.length > 2 ? parseInt(process.argv[2], 10) : 20000;
const cache = new Cache(16);
const lat: number[] = [];
let checksum = 0;
for (let i = 0; i < requests; i++) {
  const t0 = process.hrtime.bigint();
  checksum = (checksum + handle(i, cache)) % 1000000007;
  lat.push(Number(process.hrtime.bigint() - t0) / 1000);
}
console.log(`requests ${requests} checksum ${checksum}`);
lat.sort((a, b) => a - b);
const pct = (p: number): string => lat[Math.min(lat.length - 1, Math.floor(lat.length * p))].toFixed(1);
let total = 0;
for (const x of lat) {
  total += x;
}
process.stderr.write(`latency us: mean ${(total / lat.length).toFixed(1)} p50 ${pct(0.5)} p99 ${pct(0.99)} p99.9 ${pct(0.999)} max ${lat[lat.length - 1].toFixed(1)}\n`);
