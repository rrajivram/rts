// Typed port of the are-we-fast-yet benchmark of the same name (MIT license; see ../LICENSE.md).
import { Benchmark } from './benchmark.ts';
import { IdentityDictionary, Vector } from './som.ts';

class Plan extends Vector<AbstractConstraint> {
  constructor() {
    super(15);
  }

  execute(): void {
    this.forEach((c) => c.execute());
  }
}

class Sym {
  hash: number;

  constructor(hash: number) {
    this.hash = hash;
  }

  customHash(): number {
    return this.hash;
  }
}

const ABSOLUTE_STRONGEST = new Sym(0);
const REQUIRED = new Sym(1);
const STRONG_PREFERRED = new Sym(2);
const PREFERRED = new Sym(3);
const STRONG_DEFAULT = new Sym(4);
const DEFAULT = new Sym(5);
const WEAK_DEFAULT = new Sym(6);
const ABSOLUTE_WEAKEST = new Sym(7);

function createStrengthTable(): IdentityDictionary<Sym, number> {
  const strengthTable = new IdentityDictionary<Sym, number>();
  strengthTable.atPut(ABSOLUTE_STRONGEST, -10000);
  strengthTable.atPut(REQUIRED, -800);
  strengthTable.atPut(STRONG_PREFERRED, -600);
  strengthTable.atPut(PREFERRED, -400);
  strengthTable.atPut(STRONG_DEFAULT, -200);
  strengthTable.atPut(DEFAULT, 0);
  strengthTable.atPut(WEAK_DEFAULT, 500);
  strengthTable.atPut(ABSOLUTE_WEAKEST, 10000);
  return strengthTable;
}

class Strength {
  arithmeticValue: number;

  constructor(symbolicValue: Sym) {
    this.arithmeticValue = Strength.strengthTable.at(symbolicValue) as number;
  }

  sameAs(s: Strength): boolean {
    return this.arithmeticValue === s.arithmeticValue;
  }

  stronger(s: Strength): boolean {
    return this.arithmeticValue < s.arithmeticValue;
  }

  weaker(s: Strength): boolean {
    return this.arithmeticValue > s.arithmeticValue;
  }

  strongest(s: Strength): Strength {
    return s.stronger(this) ? s : this;
  }

  weakest(s: Strength): Strength {
    return s.weaker(this) ? s : this;
  }

  static of(strength: Sym): Strength {
    return Strength.strengthConstant.at(strength) as Strength;
  }

  static strengthTable: IdentityDictionary<Sym, number> = createStrengthTable();

  static createStrengthConstants(): IdentityDictionary<Sym, Strength> {
    const strengthConstant = new IdentityDictionary<Sym, Strength>();
    Strength.strengthTable.getKeys().forEach((key) => {
      strengthConstant.atPut(key, new Strength(key));
    });
    return strengthConstant;
  }

  static strengthConstant: IdentityDictionary<Sym, Strength> = Strength.createStrengthConstants();
  static absoluteWeakest: Strength = Strength.of(ABSOLUTE_WEAKEST);
  static required: Strength = Strength.of(REQUIRED);
}

abstract class AbstractConstraint {
  strength: Strength;

  constructor(strengthSym: Sym) {
    this.strength = Strength.of(strengthSym);
  }

  abstract isSatisfied(): boolean;
  abstract addToGraph(): void;
  abstract removeFromGraph(): void;
  abstract chooseMethod(mark: number): void;
  abstract inputsDo(fn: (v: Variable) => void): void;
  abstract inputsHasOne(fn: (v: Variable) => boolean): boolean;
  abstract markUnsatisfied(): void;
  abstract getOutput(): Variable;
  abstract recalculate(): void;
  abstract execute(): void;

  isInput(): boolean {
    return false;
  }

  addConstraint(planner: Planner): void {
    this.addToGraph();
    planner.incrementalAdd(this);
  }

  destroyConstraint(planner: Planner): void {
    if (this.isSatisfied()) {
      planner.incrementalRemove(this);
    }
    this.removeFromGraph();
  }

  inputsKnown(mark: number): boolean {
    return !this.inputsHasOne((v) => !(v.mark === mark || v.stay || v.determinedBy === null));
  }

  satisfy(mark: number, planner: Planner): AbstractConstraint | null {
    let overridden: AbstractConstraint | null;
    this.chooseMethod(mark);
    if (this.isSatisfied()) {
      this.inputsDo((i) => { i.mark = mark; });
      const out = this.getOutput();
      overridden = out.determinedBy;
      if (overridden !== null) {
        overridden.markUnsatisfied();
      }
      out.determinedBy = this;
      if (!planner.addPropagate(this, mark)) {
        throw new Error('Cycle encountered');
      }
      out.mark = mark;
    } else {
      overridden = null;
      if (this.strength.sameAs(Strength.required)) {
        throw new Error('Could not satisfy a required constraint');
      }
    }
    return overridden;
  }
}

abstract class BinaryConstraint extends AbstractConstraint {
  v1: Variable;
  v2: Variable;
  direction: string | null;

  constructor(var1: Variable, var2: Variable, strength: Sym, _planner: Planner) {
    super(strength);
    this.v1 = var1;
    this.v2 = var2;
    this.direction = null;
  }

  isSatisfied(): boolean {
    return this.direction !== null;
  }

  addToGraph(): void {
    this.v1.addConstraint(this);
    this.v2.addConstraint(this);
    this.direction = null;
  }

  removeFromGraph(): void {
    this.v1.removeConstraint(this);
    this.v2.removeConstraint(this);
    this.direction = null;
  }

  chooseMethod(mark: number): void {
    if (this.v1.mark === mark) {
      if (this.v2.mark !== mark && this.strength.stronger(this.v2.walkStrength)) {
        this.direction = 'forward';
        return;
      }
      this.direction = null;
      return;
    }
    if (this.v2.mark === mark) {
      if (this.v1.mark !== mark && this.strength.stronger(this.v1.walkStrength)) {
        this.direction = 'backward';
        return;
      }
      this.direction = null;
      return;
    }
    if (this.v1.walkStrength.weaker(this.v2.walkStrength)) {
      if (this.strength.stronger(this.v1.walkStrength)) {
        this.direction = 'backward';
        return;
      }
      this.direction = null;
      return;
    }
    if (this.strength.stronger(this.v2.walkStrength)) {
      this.direction = 'forward';
      return;
    }
    this.direction = null;
  }

  inputsDo(fn: (v: Variable) => void): void {
    if (this.direction === 'forward') {
      fn(this.v1);
    } else {
      fn(this.v2);
    }
  }

  inputsHasOne(fn: (v: Variable) => boolean): boolean {
    if (this.direction === 'forward') {
      return fn(this.v1);
    }
    return fn(this.v2);
  }

  markUnsatisfied(): void {
    this.direction = null;
  }

  getOutput(): Variable {
    return this.direction === 'forward' ? this.v2 : this.v1;
  }

  recalculate(): void {
    let ihn: Variable;
    let out: Variable;
    if (this.direction === 'forward') {
      ihn = this.v1; out = this.v2;
    } else {
      ihn = this.v2; out = this.v1;
    }
    out.walkStrength = this.strength.weakest(ihn.walkStrength);
    out.stay = ihn.stay;
    if (out.stay) {
      this.execute();
    }
  }
}

abstract class UnaryConstraint extends AbstractConstraint {
  output: Variable;
  satisfied: boolean;

  constructor(v: Variable, strength: Sym, planner: Planner) {
    super(strength);
    this.output = v;
    this.satisfied = false;
    this.addConstraint(planner);
  }

  isSatisfied(): boolean {
    return this.satisfied;
  }

  addToGraph(): void {
    this.output.addConstraint(this);
    this.satisfied = false;
  }

  removeFromGraph(): void {
    this.output.removeConstraint(this);
    this.satisfied = false;
  }

  chooseMethod(mark: number): void {
    this.satisfied = this.output.mark !== mark && this.strength.stronger(this.output.walkStrength);
  }

  inputsDo(_fn: (v: Variable) => void): void {}

  inputsHasOne(_fn: (v: Variable) => boolean): boolean {
    return false;
  }

  markUnsatisfied(): void {
    this.satisfied = false;
  }

  getOutput(): Variable {
    return this.output;
  }

  recalculate(): void {
    this.output.walkStrength = this.strength;
    this.output.stay = !this.isInput();
    if (this.output.stay) {
      this.execute();
    }
  }
}

class EditConstraint extends UnaryConstraint {
  isInput(): boolean {
    return true;
  }

  execute(): void {}
}

class EqualityConstraint extends BinaryConstraint {
  constructor(var1: Variable, var2: Variable, strength: Sym, planner: Planner) {
    super(var1, var2, strength, planner);
    this.addConstraint(planner);
  }

  execute(): void {
    if (this.direction === 'forward') {
      this.v2.value = this.v1.value;
    } else {
      this.v1.value = this.v2.value;
    }
  }
}

class ScaleConstraint extends BinaryConstraint {
  scale: Variable;
  offset: Variable;

  constructor(src: Variable, scale: Variable, offset: Variable, dest: Variable, strength: Sym, planner: Planner) {
    super(src, dest, strength, planner);
    this.scale = scale;
    this.offset = offset;
    this.addConstraint(planner);
  }

  addToGraph(): void {
    this.v1.addConstraint(this);
    this.v2.addConstraint(this);
    this.scale.addConstraint(this);
    this.offset.addConstraint(this);
    this.direction = null;
  }

  removeFromGraph(): void {
    this.v1.removeConstraint(this);
    this.v2.removeConstraint(this);
    this.scale.removeConstraint(this);
    this.offset.removeConstraint(this);
    this.direction = null;
  }

  execute(): void {
    if (this.direction === 'forward') {
      this.v2.value = this.v1.value * this.scale.value + this.offset.value;
    } else {
      this.v1.value = (this.v2.value - this.offset.value) / this.scale.value;
    }
  }

  inputsDo(fn: (v: Variable) => void): void {
    if (this.direction === 'forward') {
      fn(this.v1);
      fn(this.scale);
      fn(this.offset);
    } else {
      fn(this.v2);
      fn(this.scale);
      fn(this.offset);
    }
  }

  recalculate(): void {
    let ihn: Variable;
    let out: Variable;
    if (this.direction === 'forward') {
      ihn = this.v1; out = this.v2;
    } else {
      out = this.v1; ihn = this.v2;
    }
    out.walkStrength = this.strength.weakest(ihn.walkStrength);
    out.stay = ihn.stay && this.scale.stay && this.offset.stay;
    if (out.stay) {
      this.execute();
    }
  }
}

class StayConstraint extends UnaryConstraint {
  execute(): void {}
}

class Variable {
  value = 0;
  constraints: Vector<AbstractConstraint> = new Vector<AbstractConstraint>(2);
  determinedBy: AbstractConstraint | null = null;
  walkStrength: Strength = Strength.absoluteWeakest;
  stay = true;
  mark = 0;

  addConstraint(c: AbstractConstraint): void {
    this.constraints.append(c);
  }

  removeConstraint(c: AbstractConstraint): void {
    this.constraints.remove(c);
    if (this.determinedBy === c) {
      this.determinedBy = null;
    }
  }

  static value(aValue: number): Variable {
    const v = new Variable();
    v.value = aValue;
    return v;
  }
}

class Planner {
  currentMark = 1;

  newMark(): number {
    this.currentMark += 1;
    return this.currentMark;
  }

  incrementalAdd(c: AbstractConstraint): void {
    const mark = this.newMark();
    let overridden = c.satisfy(mark, this);
    while (overridden !== null) {
      overridden = overridden.satisfy(mark, this);
    }
  }

  incrementalRemove(c: AbstractConstraint): void {
    const out = c.getOutput();
    c.markUnsatisfied();
    c.removeFromGraph();
    const unsatisfied = this.removePropagateFrom(out);
    unsatisfied.forEach((u) => this.incrementalAdd(u));
  }

  extractPlanFromConstraints(constraints: Vector<AbstractConstraint>): Plan {
    const sources = new Vector<AbstractConstraint>();
    constraints.forEach((c) => {
      if (c.isInput() && c.isSatisfied()) {
        sources.append(c);
      }
    });
    return this.makePlan(sources);
  }

  makePlan(sources: Vector<AbstractConstraint>): Plan {
    const mark = this.newMark();
    const plan = new Plan();
    const todo = sources;
    while (!todo.isEmpty()) {
      const c = todo.removeFirst() as AbstractConstraint;
      if (c.getOutput().mark !== mark && c.inputsKnown(mark)) {
        plan.append(c);
        c.getOutput().mark = mark;
        this.addConstraintsConsumingTo(c.getOutput(), todo);
      }
    }
    return plan;
  }

  propagateFrom(v: Variable): void {
    const todo = new Vector<AbstractConstraint>();
    this.addConstraintsConsumingTo(v, todo);
    while (!todo.isEmpty()) {
      const c = todo.removeFirst() as AbstractConstraint;
      c.execute();
      this.addConstraintsConsumingTo(c.getOutput(), todo);
    }
  }

  addConstraintsConsumingTo(v: Variable, coll: Vector<AbstractConstraint>): void {
    const determiningC = v.determinedBy;
    v.constraints.forEach((c) => {
      if (c !== determiningC && c.isSatisfied()) {
        coll.append(c);
      }
    });
  }

  addPropagate(c: AbstractConstraint, mark: number): boolean {
    const todo = Vector.with(c);
    while (!todo.isEmpty()) {
      const d = todo.removeFirst() as AbstractConstraint;
      if (d.getOutput().mark === mark) {
        this.incrementalRemove(c);
        return false;
      }
      d.recalculate();
      this.addConstraintsConsumingTo(d.getOutput(), todo);
    }
    return true;
  }

  change(v: Variable, newValue: number): void {
    const editC = new EditConstraint(v, PREFERRED, this);
    const editV = Vector.with<AbstractConstraint>(editC);
    const plan = this.extractPlanFromConstraints(editV);
    for (let i = 0; i < 10; i += 1) {
      v.value = newValue;
      plan.execute();
    }
    editC.destroyConstraint(this);
  }

  constraintsConsuming(v: Variable, fn: (c: AbstractConstraint) => void): void {
    const determiningC = v.determinedBy;
    v.constraints.forEach((c) => {
      if (c !== determiningC && c.isSatisfied()) {
        fn(c);
      }
    });
  }

  removePropagateFrom(out: Variable): Vector<AbstractConstraint> {
    const unsatisfied = new Vector<AbstractConstraint>();
    out.determinedBy = null;
    out.walkStrength = Strength.absoluteWeakest;
    out.stay = true;
    const todo = Vector.with(out);
    while (!todo.isEmpty()) {
      const v = todo.removeFirst() as Variable;
      v.constraints.forEach((c) => {
        if (!c.isSatisfied()) { unsatisfied.append(c); }
      });
      this.constraintsConsuming(v, (c) => {
        c.recalculate();
        todo.append(c.getOutput());
      });
    }
    unsatisfied.sort((c1, c2) => c1.strength.stronger(c2.strength));
    return unsatisfied;
  }

  static chainTest(n: number): void {
    const planner = new Planner();
    const vars: Variable[] = [];
    for (let i = 0; i < n + 1; i += 1) {
      vars.push(new Variable());
    }
    for (let i = 0; i < n; i += 1) {
      const v1 = vars[i];
      const v2 = vars[i + 1];
      new EqualityConstraint(v1, v2, REQUIRED, planner);
    }
    new StayConstraint(vars[n], STRONG_DEFAULT, planner);
    const editC = new EditConstraint(vars[0], PREFERRED, planner);
    const editV = Vector.with<AbstractConstraint>(editC);
    const plan = planner.extractPlanFromConstraints(editV);
    for (let i = 0; i < 100; i += 1) {
      vars[0].value = i;
      plan.execute();
      if (vars[n].value !== i) {
        throw new Error('Chain test failed!');
      }
    }
    editC.destroyConstraint(planner);
  }

  static projectionTest(n: number): void {
    const planner = new Planner();
    const dests = new Vector<Variable>();
    const scale = Variable.value(10);
    const offset = Variable.value(1000);
    let src: Variable | null = null;
    let dst: Variable | null = null;
    for (let i = 1; i <= n; i += 1) {
      src = Variable.value(i);
      dst = Variable.value(i);
      dests.append(dst);
      new StayConstraint(src, DEFAULT, planner);
      new ScaleConstraint(src, scale, offset, dst, REQUIRED, planner);
    }
    const s = src as Variable;
    const d = dst as Variable;
    planner.change(s, 17);
    if (d.value !== 1170) {
      throw new Error('Projection test 1 failed!');
    }
    planner.change(d, 1050);
    if (s.value !== 5) {
      throw new Error('Projection test 2 failed!');
    }
    planner.change(scale, 5);
    for (let i = 0; i < n - 1; i += 1) {
      if ((dests.at(i) as Variable).value !== (i + 1) * 5 + 1000) {
        throw new Error('Projection test 3 failed!');
      }
    }
    planner.change(offset, 2000);
    for (let i = 0; i < n - 1; i += 1) {
      if ((dests.at(i) as Variable).value !== (i + 1) * 5 + 2000) {
        throw new Error('Projection test 4 failed!');
      }
    }
  }
}

class DeltaBlue extends Benchmark {
  innerBenchmarkLoop(innerIterations: number): boolean {
    Planner.chainTest(innerIterations);
    Planner.projectionTest(innerIterations);
    return true;
  }
}

export const newInstance = (): Benchmark => new DeltaBlue();
