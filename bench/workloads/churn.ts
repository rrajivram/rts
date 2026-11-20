// The hard case for reclaiming cycles by region: one long-lived cyclic
// structure that keeps losing pieces which are themselves cyclic. A tree
// whose nodes point to their parents; each step prunes a whole subtree
// (still cyclic inside: children point up) and grows a new one.
// Reports a checksum (must match Node).
//
//   rts bench/workloads/churn.ts [steps]

class TreeNode {
  parent: TreeNode | null;
  children: TreeNode[] = [];
  value: number;
  constructor(parent: TreeNode | null, value: number) {
    this.parent = parent;
    this.value = value;
  }
}

let seed = 42;
function random(): number {
  seed = (seed * 1103515245 + 12345) % 2147483648;
  return seed / 2147483648;
}

function grow(parent: TreeNode, depth: number, fanout: number): TreeNode {
  const n = new TreeNode(parent, Math.floor(random() * 1000));
  if (depth > 0) {
    for (let i = 0; i < fanout; i++) {
      n.children.push(grow(n, depth - 1, fanout));
    }
  }
  return n;
}

function sum(n: TreeNode): number {
  let s = n.value;
  for (const c of n.children) s += sum(c);
  return s;
}

const steps = Number(process.argv[2] ?? "20000");
const root = new TreeNode(null, 0);
for (let i = 0; i < 64; i++) root.children.push(grow(root, 3, 3));

let check = 0;
for (let step = 0; step < steps; step++) {
  // Prune a random subtree of the root (40 nodes) and grow a fresh one.
  const i = Math.floor(random() * root.children.length);
  root.children[i] = grow(root, 3, 3);
  if (step % 1000 === 0) check = (check + sum(root)) % 1000000007;
}
console.log("checksum", check, "nodes per subtree", 40);
