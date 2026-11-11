// node:path (POSIX).
export const sep = "/";
export const delimiter = ":";

export function normalize(p: string): string {
  if (p === "") return ".";
  const abs = p.startsWith("/");
  const trailing = p.endsWith("/");
  const out: string[] = [];
  for (const part of p.split("/")) {
    if (part === "" || part === ".") continue;
    if (part === "..") {
      if (out.length > 0 && out[out.length - 1] !== "..") out.pop();
      else if (!abs) out.push("..");
    } else out.push(part);
  }
  let r = (abs ? "/" : "") + out.join("/");
  if (r === "") r = abs ? "/" : ".";
  if (trailing && r !== "/") r += "/";
  return r;
}

export function join(...parts: string[]): string {
  const nonEmpty = parts.filter((p) => p !== "");
  return nonEmpty.length === 0 ? "." : normalize(nonEmpty.join("/"));
}

export function isAbsolute(p: string): boolean {
  return p.startsWith("/");
}

export function resolve(...parts: string[]): string {
  let r = "";
  for (let i = parts.length - 1; i >= 0 && !r.startsWith("/"); i--) {
    if (parts[i] !== "") r = r === "" ? parts[i] : parts[i] + "/" + r;
  }
  if (!r.startsWith("/")) r = __rts_cwd() + (r === "" ? "" : "/" + r);
  const n = normalize(r);
  return n.length > 1 && n.endsWith("/") ? n.slice(0, -1) : n;
}

export function dirname(p: string): string {
  if (p === "") return ".";
  let end = p.length;
  while (end > 1 && p[end - 1] === "/") end--;
  const i = p.lastIndexOf("/", end - 1);
  if (i < 0) return ".";
  if (i === 0) return "/";
  return p.slice(0, i);
}

export function basename(p: string, ext?: string): string {
  let end = p.length;
  while (end > 1 && p[end - 1] === "/") end--;
  const base = p.slice(p.lastIndexOf("/", end - 1) + 1, end);
  return ext !== undefined && base.endsWith(ext) && base !== ext ? base.slice(0, base.length - ext.length) : base;
}

export function extname(p: string): string {
  const b = basename(p);
  const i = b.lastIndexOf(".");
  return i <= 0 ? "" : b.slice(i);
}

export function relative(from: string, to: string): string {
  const a = resolve(from).split("/").filter((x) => x !== "");
  const b = resolve(to).split("/").filter((x) => x !== "");
  let i = 0;
  while (i < a.length && i < b.length && a[i] === b[i]) i++;
  const up: string[] = [];
  for (let j = i; j < a.length; j++) up.push("..");
  return up.concat(b.slice(i)).join("/");
}

export interface ParsedPath {
  root: string;
  dir: string;
  base: string;
  ext: string;
  name: string;
}

export function parse(p: string): ParsedPath {
  const root = p.startsWith("/") ? "/" : "";
  const base = basename(p);
  const ext = extname(p);
  const dir = p.includes("/") ? dirname(p) : "";
  return { root, dir, base, ext, name: ext === "" ? base : base.slice(0, base.length - ext.length) };
}

export function format(o: { dir?: string; root?: string; base?: string; name?: string; ext?: string }): string {
  const dir = o.dir ?? o.root ?? "";
  const base = o.base ?? (o.name ?? "") + (o.ext ?? "");
  return dir === "" ? base : dir === o.root ? dir + base : dir + "/" + base;
}

export const posix = { sep, delimiter, normalize, join, isAbsolute, resolve, dirname, basename, extname, relative, parse, format };
export default posix;
