import * as fs from "node:fs";
import path from "node:path";
import { readFileSync } from "fs";
import { EventEmitter } from "node:events";
import { format, inspect } from "node:util";
import os from "node:os";
import assert from "node:assert";

const dir = path.join(os.tmpdir(), "rts-node-builtins-test");
fs.rmSync(dir, { recursive: true, force: true });
fs.mkdirSync(dir, { recursive: true });
const file = path.join(dir, "a.txt");
fs.writeFileSync(file, "hello\nworld");
fs.appendFileSync(file, "!");
console.log(readFileSync(file, "utf8").split("\n"), fs.existsSync(file), fs.statSync(file).size, fs.readdirSync(dir));
console.log(path.basename(file), path.extname(file), path.basename(path.dirname(file)), path.join("a", "..", "b", "./c"));
console.log(path.relative("/a/b/c", "/a/d"), path.normalize("/x//y/../z/"), path.parse("/home/u/f.tar.gz"));
try {
  fs.readFileSync(path.join(dir, "missing.txt"), "utf8");
} catch (e: any) {
  console.log(e.code, e.message.includes("no such file or directory"));
}
fs.rmSync(dir, { recursive: true });
console.log(fs.existsSync(dir));

const em = new EventEmitter();
em.on("data", (x: number) => console.log("data", x));
em.once("data", () => console.log("first only"));
em.emit("data", 1);
em.emit("data", 2);
console.log(em.listenerCount("data"), em.emit("none"));

console.log(format("%s has %d items (%i%%) %j", "cart", 3.5, 42.9, { a: 1 }), inspect({ a: { b: { c: { d: 1 } } } }, { depth: 0 }));
console.log("%s-%s", "a", "b", "extra");
assert.strictEqual(1 + 1, 2);
assert.deepStrictEqual({ a: [1, 2] }, { a: [1, 2] });
try {
  assert.strictEqual(1, 2);
} catch (e: any) {
  console.log(e.name, e.code);
}
console.log(typeof process.argv[1], process.argv.length >= 2, typeof process.env.PATH, process.platform === os.platform());
const d = new Date(Date.UTC(2024, 1, 29, 12, 30, 15, 250));
console.log(d.toISOString(), d.getUTCDay(), JSON.stringify({ d }), d.getTime(), new Date("2024-03-01T00:00:00Z").getTime() - d.getTime());
console.log(new Date("nonsense").getTime(), new Date(0), Date.UTC(1970, 0, 2));
