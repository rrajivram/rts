// Characters of strings (shared one-character strings) and `===` on
// strings in compiled code: same object, different lengths, one byte.

function countChar(s: string, c: string): number {
  let n = 0;
  for (let i = 0; i < s.length; i++) if (s[i] === c) n++;
  return n;
}

function eqs(a: string, b: string): string {
  return (a === b ? "T" : "F") + (a !== b ? "t" : "f");
}

const text = "the quick brown fox jumps over the lazy dog";
let total = 0;
for (let r = 0; r < 100; r++) total += countChar(text, "o") + countChar(text, " ");
console.log(total);
console.log(countChar("héllo wörld", "l"), countChar("héllo wörld", "ö"), countChar("aaa", ""), countChar("", "a"));
console.log(eqs("a", "a"), eqs("a", "b"), eqs("ab", "a"), eqs("", ""), eqs("", "x"), eqs("é", "é"), eqs("é", "e"));
console.log(eqs("abc".charAt(1), "b"), eqs("abc".at(-1)!, "c"), eqs("abc".charAt(9), ""), eqs("xyz"[0], "x"));
const parts = "a,b".split("");
console.log(parts.length, eqs(parts[1], ","), [..."hé!"].join("|"));
const kinds: string[] = [];
for (const v of [1, "s", true, null, undefined, {}, () => 1, 10n]) kinds.push(typeof v);
console.log(kinds.join(" "), typeof 1 === "number", typeof "x" === typeof "y");
let s = "";
for (let i = 0; i < 26; i++) s += String.fromCharCode(97 + i);
let hits = 0;
for (const ch of s) if (ch === "e" || ch === "z") hits++;
console.log(hits, s.length, eqs(s.slice(0, 3), "abc"), eqs(s.slice(0, 3), "abd"));
