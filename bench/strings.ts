// String building, splitting and a word-frequency Map.
const words = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta"];
const parts: string[] = [];
let seed = 7;
for (let i = 0; i < 200_000; i++) {
  seed = (seed * 1103515245 + 12345) % 2147483648;
  parts.push(words[seed % words.length]);
}
const text = parts.join(" ");
const freq = new Map<string, number>();
for (const w of text.split(" ")) freq.set(w, (freq.get(w) ?? 0) + 1);
let longest = "";
for (const [w, n] of freq) if (w.length > longest.length) longest = w;
let codes = 0;
for (let i = 0; i < text.length; i++) codes += text.charCodeAt(i);
console.log(freq.size, longest, codes, text.length);
