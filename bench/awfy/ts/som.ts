// Typed port of are-we-fast-yet's som.js (MIT license, Copyright (c)
// 2015-2016 Stefan Marr; see ../LICENSE.md). Same algorithms, with types.

const INITIAL_SIZE = 10;
const INITIAL_CAPACITY = 16;

export class Vector<T> {
  storage: (T | null)[] | null;
  firstIdx: number;
  lastIdx: number;

  constructor(size?: number) {
    this.storage = size === undefined || size === 0 ? null : new Array<T | null>(size).fill(null);
    this.firstIdx = 0;
    this.lastIdx = 0;
  }

  static with<T>(elem: T): Vector<T> {
    const v = new Vector<T>(1);
    v.append(elem);
    return v;
  }

  at(idx: number): T | null {
    if (this.storage === null || idx >= this.storage.length) {
      return null;
    }
    return this.storage[idx];
  }

  atPut(idx: number, val: T): void {
    if (this.storage === null) {
      this.storage = new Array<T | null>(Math.max(idx + 1, INITIAL_SIZE)).fill(null);
    } else if (idx >= this.storage.length) {
      let newLength = this.storage.length;
      while (newLength <= idx) {
        newLength *= 2;
      }
      this.storage = grow(this.storage, newLength);
    }
    this.storage[idx] = val;
    if (this.lastIdx < idx + 1) {
      this.lastIdx = idx + 1;
    }
  }

  append(elem: T): void {
    if (this.storage === null) {
      this.storage = new Array<T | null>(INITIAL_SIZE).fill(null);
    } else if (this.lastIdx >= this.storage.length) {
      this.storage = grow(this.storage, this.storage.length * 2);
    }
    this.storage[this.lastIdx] = elem;
    this.lastIdx += 1;
  }

  isEmpty(): boolean {
    return this.lastIdx === this.firstIdx;
  }

  forEach(fn: (e: T) => void): void {
    const s = this.storage as (T | null)[];
    for (let i = this.firstIdx; i < this.lastIdx; i += 1) {
      fn(s[i] as T);
    }
  }

  hasSome(fn: (e: T) => boolean): boolean {
    const s = this.storage as (T | null)[];
    for (let i = this.firstIdx; i < this.lastIdx; i += 1) {
      if (fn(s[i] as T)) {
        return true;
      }
    }
    return false;
  }

  getOne(fn: (e: T) => boolean): T | null {
    const s = this.storage as (T | null)[];
    for (let i = this.firstIdx; i < this.lastIdx; i += 1) {
      const e = s[i] as T;
      if (fn(e)) {
        return e;
      }
    }
    return null;
  }

  removeFirst(): T | null {
    if (this.isEmpty()) {
      return null;
    }
    this.firstIdx += 1;
    return (this.storage as (T | null)[])[this.firstIdx - 1];
  }

  remove(obj: T): boolean {
    if (this.storage === null || this.isEmpty()) {
      return false;
    }
    const newArray = new Array<T | null>(this.capacity()).fill(null);
    let newLast = 0;
    let found = false;
    this.forEach((it) => {
      if (it === obj) {
        found = true;
      } else {
        newArray[newLast] = it;
        newLast += 1;
      }
    });
    this.storage = newArray;
    this.lastIdx = newLast;
    this.firstIdx = 0;
    return found;
  }

  removeAll(): void {
    this.firstIdx = 0;
    this.lastIdx = 0;
    if (this.storage !== null) {
      this.storage = new Array<T | null>(this.storage.length).fill(null);
    }
  }

  size(): number {
    return this.lastIdx - this.firstIdx;
  }

  capacity(): number {
    return this.storage === null ? 0 : this.storage.length;
  }

  // As upstream: swap and defaultSort are unimplemented, so sorting only
  // works while no swap is needed.
  swap(_storage: (T | null)[], _i: number, _j: number): void {
    throw new Error('Not Implemented');
  }

  defaultSort(_i: number, _j: number): void {
    throw new Error('Not Implemented');
  }

  sortRange(i: number, j: number, compare: ((a: T, b: T) => boolean) | null): void {
    if (!compare) {
      this.defaultSort(i, j);
      return;
    }
    const s = this.storage as (T | null)[];
    const n = j + 1 - i;
    if (n <= 1) {
      return;
    }
    let di = s[i] as T;
    let dj = s[j] as T;
    if (compare(di, dj)) {
      this.swap(s, i, j);
      const tt = di;
      di = dj;
      dj = tt;
    }
    if (n > 2) {
      const ij = (i + j) / 2;
      let dij = s[ij] as T;
      if (Number(compare(di, dij)) <= 0) {
        if (!compare(dij, dj)) {
          this.swap(s, j, ij);
          dij = dj;
        }
      } else {
        this.swap(s, i, ij);
        dij = di;
      }
      if (n > 3) {
        let k = i;
        let l = j - 1;
        while (true) {
          while (k <= l && compare(dij, s[l] as T)) {
            l -= 1;
          }
          k += 1;
          while (k <= l && compare(s[k] as T, dij)) {
            k += 1;
          }
          if (k > l) {
            break;
          }
          this.swap(s, k, l);
        }
        this.sortRange(i, l, null);
        this.sortRange(k, j, null);
      }
    }
  }

  sort(compare: (a: T, b: T) => boolean): void {
    if (this.size() > 0) {
      this.sortRange(this.firstIdx, this.lastIdx - 1, compare);
    }
  }
}

function grow<T>(a: (T | null)[], n: number): (T | null)[] {
  const b = new Array<T | null>(n).fill(null);
  for (let i = 0; i < a.length; i += 1) {
    b[i] = a[i];
  }
  return b;
}

export class Set<T> {
  items: Vector<T>;

  constructor(size?: number) {
    this.items = new Vector<T>(size === undefined ? INITIAL_SIZE : size);
  }

  size(): number {
    return this.items.size();
  }

  forEach(fn: (e: T) => void): void {
    this.items.forEach(fn);
  }

  hasSome(fn: (e: T) => boolean): boolean {
    return this.items.hasSome(fn);
  }

  getOne(fn: (e: T) => boolean): T | null {
    return this.items.getOne(fn);
  }

  add(obj: T): void {
    if (!this.contains(obj)) {
      this.items.append(obj);
    }
  }

  contains(obj: T): boolean {
    return this.hasSome((e) => e === obj);
  }

  removeAll(): void {
    this.items.removeAll();
  }

  collect<U>(fn: (e: T) => U): Vector<U> {
    const coll = new Vector<U>();
    this.forEach((e) => coll.append(fn(e)));
    return coll;
  }
}

export class IdentitySet<T> extends Set<T> {
  constructor(size?: number) {
    super(size === undefined ? INITIAL_SIZE : size);
  }

  contains(obj: T): boolean {
    return this.hasSome((e) => e === obj);
  }
}

export interface CustomHash {
  customHash(): number;
}

class DictEntry<K, V> {
  hash: number;
  key: K;
  value: V;
  next: DictEntry<K, V> | null;

  constructor(hash: number, key: K, value: V, next: DictEntry<K, V> | null) {
    this.hash = hash;
    this.key = key;
    this.value = value;
    this.next = next;
  }

  match(hash: number, key: K): boolean {
    return this.hash === hash && key === this.key;
  }
}

function hashFn(key: CustomHash | null): number {
  if (!key) {
    return 0;
  }
  const hash = key.customHash();
  return hash ^ (hash >>> 16);
}

export class Dictionary<K extends CustomHash, V> {
  buckets: (DictEntry<K, V> | null)[];
  size_: number;

  constructor(size?: number) {
    this.buckets = new Array<DictEntry<K, V> | null>(size === undefined ? INITIAL_CAPACITY : size).fill(null);
    this.size_ = 0;
  }

  size(): number {
    return this.size_;
  }

  isEmpty(): boolean {
    return this.size_ === 0;
  }

  getBucketIdx(hash: number): number {
    return (this.buckets.length - 1) & hash;
  }

  getBucket(hash: number): DictEntry<K, V> | null {
    return this.buckets[this.getBucketIdx(hash)];
  }

  at(key: K): V | null {
    const hash = hashFn(key);
    let e = this.getBucket(hash);
    while (e) {
      if (e.match(hash, key)) {
        return e.value;
      }
      e = e.next;
    }
    return null;
  }

  containsKey(key: K): boolean {
    const hash = hashFn(key);
    let e = this.getBucket(hash);
    while (e) {
      if (e.match(hash, key)) {
        return true;
      }
      e = e.next;
    }
    return false;
  }

  atPut(key: K, value: V): void {
    const hash = hashFn(key);
    const i = this.getBucketIdx(hash);
    const current = this.buckets[i];
    if (!current) {
      this.buckets[i] = this.newEntry(key, value, hash);
      this.size_ += 1;
    } else {
      this.insertBucketEntry(key, value, hash, current);
    }
    if (this.size_ > this.buckets.length) {
      this.resize();
    }
  }

  newEntry(key: K, value: V, hash: number): DictEntry<K, V> {
    return new DictEntry<K, V>(hash, key, value, null);
  }

  insertBucketEntry(key: K, value: V, hash: number, head: DictEntry<K, V>): void {
    let current = head;
    while (true) {
      if (current.match(hash, key)) {
        current.value = value;
        return;
      }
      const next = current.next;
      if (!next) {
        this.size_ += 1;
        current.next = this.newEntry(key, value, hash);
        return;
      }
      current = next;
    }
  }

  resize(): void {
    const oldStorage = this.buckets;
    this.buckets = new Array<DictEntry<K, V> | null>(oldStorage.length * 2).fill(null);
    this.transferEntries(oldStorage);
  }

  transferEntries(oldStorage: (DictEntry<K, V> | null)[]): void {
    for (let i = 0; i < oldStorage.length; i += 1) {
      const current = oldStorage[i];
      if (current) {
        oldStorage[i] = null;
        if (!current.next) {
          this.buckets[current.hash & (this.buckets.length - 1)] = current;
        } else {
          this.splitBucket(oldStorage, i, current);
        }
      }
    }
  }

  splitBucket(oldStorage: (DictEntry<K, V> | null)[], i: number, head: DictEntry<K, V>): void {
    let loHead: DictEntry<K, V> | null = null;
    let loTail: DictEntry<K, V> | null = null;
    let hiHead: DictEntry<K, V> | null = null;
    let hiTail: DictEntry<K, V> | null = null;
    let current: DictEntry<K, V> | null = head;
    while (current) {
      if ((current.hash & oldStorage.length) === 0) {
        if (!loTail) {
          loHead = current;
        } else {
          loTail.next = current;
        }
        loTail = current;
      } else {
        if (!hiTail) {
          hiHead = current;
        } else {
          hiTail.next = current;
        }
        hiTail = current;
      }
      current = current.next;
    }
    if (loTail) {
      loTail.next = null;
      this.buckets[i] = loHead;
    }
    if (hiTail) {
      hiTail.next = null;
      this.buckets[i + oldStorage.length] = hiHead;
    }
  }

  removeAll(): void {
    this.buckets = new Array<DictEntry<K, V> | null>(this.buckets.length).fill(null);
    this.size_ = 0;
  }

  getKeys(): Vector<K> {
    const keys = new Vector<K>(this.size_);
    for (let i = 0; i < this.buckets.length; i += 1) {
      let current = this.buckets[i];
      while (current) {
        keys.append(current.key);
        current = current.next;
      }
    }
    return keys;
  }

  getValues(): Vector<V> {
    const values = new Vector<V>(this.size_);
    for (let i = 0; i < this.buckets.length; i += 1) {
      let current = this.buckets[i];
      while (current) {
        values.append(current.value);
        current = current.next;
      }
    }
    return values;
  }
}

class DictIdEntry<K, V> extends DictEntry<K, V> {
  match(hash: number, key: K): boolean {
    return this.hash === hash && this.key === key;
  }
}

export class IdentityDictionary<K extends CustomHash, V> extends Dictionary<K, V> {
  constructor(size?: number) {
    super(size === undefined ? INITIAL_CAPACITY : size);
  }

  newEntry(key: K, value: V, hash: number): DictEntry<K, V> {
    return new DictIdEntry<K, V>(hash, key, value, null);
  }
}

export class Random {
  seed: number;

  constructor() {
    this.seed = 74755;
  }

  next(): number {
    this.seed = (this.seed * 1309 + 13849) & 65535;
    return this.seed;
  }
}
