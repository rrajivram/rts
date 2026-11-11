// BigInt arithmetic: modular exponentiation and a hash, mod the Mersenne
// prime 2^61 - 1 (products stay under 2^122, inside rts's 128-bit BigInts).
const P: bigint = (1n << 61n) - 1n;

function modpow(base: bigint, exp: bigint): bigint {
  let result = 1n;
  base %= P;
  while (exp > 0n) {
    if (exp & 1n) result = (result * base) % P;
    base = (base * base) % P;
    exp >>= 1n;
  }
  return result;
}

function hash(n: number): bigint {
  let h = 0n;
  let x = 12345n;
  for (let i = 0; i < n; i++) {
    x = (x * 6364136223846793005n + 1442695040888963407n) % P;
    h = (h * 31n + x) % P;
  }
  return h;
}

let acc = 0n;
for (let i = 1n; i <= 20000n; i++) {
  acc = (acc + modpow(i, P - 2n)) % P;
}
console.log(acc);
console.log(hash(300000));
