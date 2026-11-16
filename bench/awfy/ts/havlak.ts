// Typed port of the are-we-fast-yet benchmark of the same name (MIT license; see ../LICENSE.md).
// Upstream's ES5 constructor function BasicBlockEdge is a class here.
import { Benchmark } from './benchmark.ts';
import { IdentityDictionary, IdentitySet, Set as SomSet, Vector } from './som.ts';

class BasicBlock {
  name: number;
  inEdges: Vector<BasicBlock> = new Vector<BasicBlock>(2);
  outEdges: Vector<BasicBlock> = new Vector<BasicBlock>(2);

  constructor(name: number) {
    this.name = name;
  }

  getInEdges(): Vector<BasicBlock> {
    return this.inEdges;
  }

  getOutEdges(): Vector<BasicBlock> {
    return this.outEdges;
  }

  getNumPred(): number {
    return this.inEdges.size();
  }

  addOutEdge(to: BasicBlock): void {
    this.outEdges.append(to);
  }

  addInEdge(from: BasicBlock): void {
    this.inEdges.append(from);
  }

  customHash(): number {
    return this.name;
  }
}

class BasicBlockEdge {
  from: BasicBlock;
  to: BasicBlock;

  constructor(cfg: ControlFlowGraph, fromName: number, toName: number) {
    this.from = cfg.createNode(fromName);
    this.to = cfg.createNode(toName);
    this.from.addOutEdge(this.to);
    this.to.addInEdge(this.from);
    cfg.addEdge(this);
  }
}

class ControlFlowGraph {
  startNode: BasicBlock | null = null;
  basicBlockMap: Vector<BasicBlock> = new Vector<BasicBlock>();
  edgeList: Vector<BasicBlockEdge> = new Vector<BasicBlockEdge>();

  createNode(name: number): BasicBlock {
    let node: BasicBlock;
    const existing = this.basicBlockMap.at(name);
    if (existing) {
      node = existing;
    } else {
      node = new BasicBlock(name);
      this.basicBlockMap.atPut(name, node);
    }
    if (this.getNumNodes() === 1) {
      this.startNode = node;
    }
    return node;
  }

  addEdge(edge: BasicBlockEdge): void {
    this.edgeList.append(edge);
  }

  getNumNodes(): number {
    return this.basicBlockMap.size();
  }

  getStartBasicBlock(): BasicBlock | null {
    return this.startNode;
  }

  getBasicBlocks(): Vector<BasicBlock> {
    return this.basicBlockMap;
  }
}

class SimpleLoop {
  isReducible: boolean;
  parent: SimpleLoop | null = null;
  isRoot_ = false;
  nestingLevel = 0;
  depthLevel = 0;
  counter = 0;
  basicBlocks: IdentitySet<BasicBlock> = new IdentitySet<BasicBlock>();
  children: IdentitySet<SimpleLoop> = new IdentitySet<SimpleLoop>();
  header: BasicBlock | null;

  constructor(bb: BasicBlock | null, isReducible: boolean) {
    this.isReducible = isReducible;
    if (bb) {
      this.basicBlocks.add(bb);
    }
    this.header = bb;
  }

  addNode(bb: BasicBlock): void {
    this.basicBlocks.add(bb);
  }

  addChildLoop(loop: SimpleLoop): void {
    this.children.add(loop);
  }

  getChildren(): IdentitySet<SimpleLoop> {
    return this.children;
  }

  getParent(): SimpleLoop | null {
    return this.parent;
  }

  getNestingLevel(): number {
    return this.nestingLevel;
  }

  isRoot(): boolean {
    return this.isRoot_;
  }

  setParent(parent: SimpleLoop): void {
    this.parent = parent;
    parent.addChildLoop(this);
  }

  setIsRoot(): void {
    this.isRoot_ = true;
  }

  setCounter(value: number): void {
    this.counter = value;
  }

  setNestingLevel(level: number): void {
    this.nestingLevel = level;
    if (level === 0) {
      this.setIsRoot();
    }
  }

  setDepthLevel(level: number): void {
    this.depthLevel = level;
  }
}

class LoopStructureGraph {
  loopCounter = 0;
  loops: Vector<SimpleLoop> = new Vector<SimpleLoop>();
  root: SimpleLoop;

  constructor() {
    this.root = new SimpleLoop(null, true);
    this.root.setNestingLevel(0);
    this.root.setCounter(this.loopCounter);
    this.loopCounter += 1;
    this.loops.append(this.root);
  }

  createNewLoop(bb: BasicBlock, isReducible: boolean): SimpleLoop {
    const loop = new SimpleLoop(bb, isReducible);
    loop.setCounter(this.loopCounter);
    this.loopCounter += 1;
    this.loops.append(loop);
    return loop;
  }

  calculateNestingLevel(): void {
    // Link up all 1st level loops to artificial root node.
    this.loops.forEach((liter) => {
      if (!liter.isRoot()) {
        if (!liter.getParent()) {
          liter.setParent(this.root);
        }
      }
    });
    // Recursively traverse the tree and assign levels.
    this.calculateNestingLevelRec(this.root, 0);
  }

  calculateNestingLevelRec(loop: SimpleLoop, depth: number): void {
    loop.setDepthLevel(depth);
    loop.getChildren().forEach((liter) => {
      this.calculateNestingLevelRec(liter, depth + 1);
      loop.setNestingLevel(Math.max(loop.getNestingLevel(), 1 + liter.getNestingLevel()));
    });
  }

  getNumLoops(): number {
    return this.loops.size();
  }
}

class UnionFindNode {
  parent: UnionFindNode;
  bb: BasicBlock | null = null;
  dfsNumber = 0;
  loop: SimpleLoop | null = null;

  constructor() {
    this.parent = this;
  }

  initNode(bb: BasicBlock, dfsNumber: number): void {
    this.parent = this;
    this.bb = bb;
    this.dfsNumber = dfsNumber;
    this.loop = null;
  }

  findSet(): UnionFindNode {
    const nodeList = new Vector<UnionFindNode>();
    let node: UnionFindNode = this;
    while (node !== node.parent) {
      if (node.parent !== node.parent.parent) {
        nodeList.append(node);
      }
      node = node.parent;
    }
    // Path Compression, all nodes' parents point to the 1st level parent.
    nodeList.forEach((iter) => iter.union(this.parent));
    return node;
  }

  union(basicBlock: UnionFindNode): void {
    this.parent = basicBlock;
  }

  getBb(): BasicBlock | null {
    return this.bb;
  }

  getLoop(): SimpleLoop | null {
    return this.loop;
  }

  getDfsNumber(): number {
    return this.dfsNumber;
  }

  setLoop(loop: SimpleLoop): void {
    this.loop = loop;
  }
}

const UNVISITED = 2147483647;
const MAXNONBACKPREDS = (32 * 1024);

class HavlakLoopFinder {
  nonBackPreds: Vector<SomSet<number>> = new Vector<SomSet<number>>();
  backPreds: Vector<Vector<number>> = new Vector<Vector<number>>();
  number: IdentityDictionary<BasicBlock, number> = new IdentityDictionary<BasicBlock, number>();
  maxSize = 0;
  header: number[] = [];
  type: string[] = [];
  last: number[] = [];
  nodes: UnionFindNode[] = [];
  cfg: ControlFlowGraph;
  lsg: LoopStructureGraph;

  constructor(cfg: ControlFlowGraph, lsg: LoopStructureGraph) {
    this.cfg = cfg;
    this.lsg = lsg;
  }

  isAncestor(w: number, v: number): boolean {
    return w <= v && v <= this.last[w];
  }

  doDFS(currentNode: BasicBlock, current: number): number {
    this.nodes[current].initNode(currentNode, current);
    this.number.atPut(currentNode, current);
    let lastId = current;
    const outerBlocks = currentNode.getOutEdges();
    for (let i = 0; i < outerBlocks.size(); i += 1) {
      const target = outerBlocks.at(i)!;
      if (this.number.at(target) === UNVISITED) {
        lastId = this.doDFS(target, lastId + 1);
      }
    }
    this.last[current] = lastId;
    return lastId;
  }

  initAllNodes(): void {
    this.cfg.getBasicBlocks().forEach((bb) => { this.number.atPut(bb, UNVISITED); });
    this.doDFS(this.cfg.getStartBasicBlock()!, 0);
  }

  identifyEdges(size: number): void {
    for (let w = 0; w < size; w += 1) {
      this.header[w] = 0;
      this.type[w] = 'BB_NONHEADER';
      const nodeW = this.nodes[w].getBb();
      if (!nodeW) {
        this.type[w] = 'BB_DEAD';
      } else {
        this.processEdges(nodeW, w);
      }
    }
  }

  processEdges(nodeW: BasicBlock, w: number): void {
    if (nodeW.getNumPred() > 0) {
      nodeW.getInEdges().forEach((nodeV) => {
        const v = this.number.at(nodeV)!;
        if (v !== UNVISITED) {
          if (this.isAncestor(w, v)) {
            this.backPreds.at(w)!.append(v);
          } else {
            this.nonBackPreds.at(w)!.add(v);
          }
        }
      });
    }
  }

  findLoops(): void {
    if (!this.cfg.getStartBasicBlock()) {
      return;
    }
    const size = this.cfg.getNumNodes();
    this.nonBackPreds.removeAll();
    this.backPreds.removeAll();
    this.number.removeAll();
    if (size > this.maxSize) {
      this.header = new Array<number>(size).fill(0);
      this.type = new Array<string>(size).fill('');
      this.last = new Array<number>(size).fill(0);
      this.nodes = [];
      this.maxSize = size;
    }
    for (let i = 0; i < size; i += 1) {
      this.nonBackPreds.append(new SomSet<number>());
      this.backPreds.append(new Vector<number>());
      this.nodes[i] = new UnionFindNode();
    }
    this.initAllNodes();
    this.identifyEdges(size);
    this.header[0] = 0;
    for (let w = size - 1; w >= 0; w -= 1) {
      const nodePool = new Vector<UnionFindNode>();
      const nodeW = this.nodes[w].getBb();
      if (nodeW) {
        this.stepD(w, nodePool);
        const workList = new Vector<UnionFindNode>();
        nodePool.forEach((niter) => workList.append(niter));
        if (nodePool.size() !== 0) {
          this.type[w] = 'BB_REDUCIBLE';
        }
        while (!workList.isEmpty()) {
          const x = workList.removeFirst()!;
          const nonBackSize = this.nonBackPreds.at(x.getDfsNumber())!.size();
          if (nonBackSize > MAXNONBACKPREDS) {
            return;
          }
          this.stepEProcessNonBackPreds(w, nodePool, workList, x);
        }
        if ((nodePool.size() > 0) || (this.type[w] === 'BB_SELF')) {
          const loop = this.lsg.createNewLoop(nodeW, this.type[w] !== 'BB_IRREDUCIBLE');
          this.setLoopAttributes(w, nodePool, loop);
        }
      }
    }
  }

  stepEProcessNonBackPreds(w: number, nodePool: Vector<UnionFindNode>, workList: Vector<UnionFindNode>, x: UnionFindNode): void {
    this.nonBackPreds.at(x.getDfsNumber())!.forEach((iter) => {
      const y = this.nodes[iter];
      const ydash = y.findSet();
      if (!this.isAncestor(w, ydash.getDfsNumber())) {
        this.type[w] = 'BB_IRREDUCIBLE';
        this.nonBackPreds.at(w)!.add(ydash.getDfsNumber());
      } else if (ydash.getDfsNumber() !== w) {
        if (!nodePool.hasSome((e) => e === ydash)) {
          workList.append(ydash);
          nodePool.append(ydash);
        }
      }
    });
  }

  setLoopAttributes(w: number, nodePool: Vector<UnionFindNode>, loop: SimpleLoop): void {
    this.nodes[w].setLoop(loop);
    nodePool.forEach((node) => {
      this.header[node.getDfsNumber()] = w;
      node.union(this.nodes[w]);
      const l = node.getLoop();
      if (l) {
        l.setParent(loop);
      } else {
        loop.addNode(node.getBb()!);
      }
    });
  }

  stepD(w: number, nodePool: Vector<UnionFindNode>): void {
    this.backPreds.at(w)!.forEach((v) => {
      if (v !== w) {
        nodePool.append(this.nodes[v].findSet());
      } else {
        this.type[w] = 'BB_SELF';
      }
    });
  }
}

class LoopTesterApp {
  cfg: ControlFlowGraph = new ControlFlowGraph();
  lsg: LoopStructureGraph = new LoopStructureGraph();

  constructor() {
    this.cfg.createNode(0);
  }

  buildDiamond(start: number): number {
    const bb0 = start;
    new BasicBlockEdge(this.cfg, bb0, bb0 + 1);
    new BasicBlockEdge(this.cfg, bb0, bb0 + 2);
    new BasicBlockEdge(this.cfg, bb0 + 1, bb0 + 3);
    new BasicBlockEdge(this.cfg, bb0 + 2, bb0 + 3);
    return bb0 + 3;
  }

  buildConnect(start: number, end: number): void {
    new BasicBlockEdge(this.cfg, start, end);
  }

  buildStraight(start: number, n: number): number {
    for (let i = 0; i < n; i += 1) {
      this.buildConnect(start + i, start + i + 1);
    }
    return start + n;
  }

  buildBaseLoop(from: number): number {
    const header = this.buildStraight(from, 1);
    const diamond1 = this.buildDiamond(header);
    const d11 = this.buildStraight(diamond1, 1);
    const diamond2 = this.buildDiamond(d11);
    let footer = this.buildStraight(diamond2, 1);
    this.buildConnect(diamond2, d11);
    this.buildConnect(diamond1, header);
    this.buildConnect(footer, from);
    footer = this.buildStraight(footer, 1);
    return footer;
  }

  main(numDummyLoops: number, findLoopIterations: number, parLoops: number, pparLoops: number, ppparLoops: number): number[] {
    this.constructSimpleCFG();
    this.addDummyLoops(numDummyLoops);
    this.constructCFG(parLoops, pparLoops, ppparLoops);
    this.findLoops(this.lsg);
    for (let i = 0; i < findLoopIterations; i += 1) {
      this.findLoops(new LoopStructureGraph());
    }
    this.lsg.calculateNestingLevel();
    return [this.lsg.getNumLoops(), this.cfg.getNumNodes()];
  }

  constructCFG(parLoops: number, pparLoops: number, ppparLoops: number): void {
    let n = 2;
    for (let parlooptrees = 0; parlooptrees < parLoops; parlooptrees += 1) {
      this.cfg.createNode(n + 1);
      this.buildConnect(2, n + 1);
      n += 1;
      for (let i = 0; i < pparLoops; i += 1) {
        const top = n;
        n = this.buildStraight(n, 1);
        for (let j = 0; j < ppparLoops; j += 1) {
          n = this.buildBaseLoop(n);
        }
        const bottom = this.buildStraight(n, 1);
        this.buildConnect(n, top);
        n = bottom;
      }
      this.buildConnect(n, 1);
    }
  }

  addDummyLoops(numDummyLoops: number): void {
    for (let dummyloop = 0; dummyloop < numDummyLoops; dummyloop += 1) {
      this.findLoops(this.lsg);
    }
  }

  findLoops(loopStructure: LoopStructureGraph): void {
    const finder = new HavlakLoopFinder(this.cfg, loopStructure);
    finder.findLoops();
  }

  constructSimpleCFG(): void {
    this.cfg.createNode(0);
    this.buildBaseLoop(0);
    this.cfg.createNode(1);
    new BasicBlockEdge(this.cfg, 0, 2);
  }
}

class Havlak extends Benchmark {
  innerBenchmarkLoop(innerIterations: number): boolean {
    return this.verifyResult2(
      (new LoopTesterApp()).main(innerIterations, 50, 10, 10, 5),
      innerIterations,
    );
  }

  verifyResult2(result: number[], innerIterations: number): boolean {
    if (innerIterations === 15000) { return result[0] === 46602 && result[1] === 5213; }
    if (innerIterations === 1500) { return result[0] === 6102 && result[1] === 5213; }
    if (innerIterations === 150) { return result[0] === 2052 && result[1] === 5213; }
    if (innerIterations === 15) { return result[0] === 1647 && result[1] === 5213; }
    if (innerIterations === 1) { return result[0] === 1605 && result[1] === 5213; }
    process.stdout.write(`No verification result for ${innerIterations} found`);
    process.stdout.write(`Result is: ${result[0]}, ${result[1]}`);
    return false;
  }
}

export const newInstance = (): Benchmark => new Havlak();
