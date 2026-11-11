// node:events
type Listener = (...args: any[]) => void;

export class EventEmitter {
  private listenersMap: Map<string | symbol, { fn: Listener; once: boolean }[]> = new Map();
  static defaultMaxListeners = 10;

  on(event: string | symbol, fn: Listener): this {
    return this.add(event, fn, false, false);
  }
  addListener(event: string | symbol, fn: Listener): this {
    return this.add(event, fn, false, false);
  }
  prependListener(event: string | symbol, fn: Listener): this {
    return this.add(event, fn, false, true);
  }
  once(event: string | symbol, fn: Listener): this {
    return this.add(event, fn, true, false);
  }
  private add(event: string | symbol, fn: Listener, once: boolean, prepend: boolean): this {
    if (this.listenersMap.has("newListener") && event !== "newListener") this.emit("newListener", event, fn);
    let list = this.listenersMap.get(event);
    if (list === undefined) {
      list = [];
      this.listenersMap.set(event, list);
    }
    if (prepend) list.unshift({ fn, once });
    else list.push({ fn, once });
    return this;
  }
  off(event: string | symbol, fn: Listener): this {
    return this.removeListener(event, fn);
  }
  removeListener(event: string | symbol, fn: Listener): this {
    const list = this.listenersMap.get(event);
    if (list !== undefined) {
      const i = list.findIndex((l) => l.fn === fn);
      if (i >= 0) list.splice(i, 1);
      if (list.length === 0) this.listenersMap.delete(event);
    }
    return this;
  }
  removeAllListeners(event?: string | symbol): this {
    if (event === undefined) this.listenersMap.clear();
    else this.listenersMap.delete(event);
    return this;
  }
  emit(event: string | symbol, ...args: any[]): boolean {
    const list = this.listenersMap.get(event);
    if (list === undefined || list.length === 0) {
      if (event === "error") throw args[0];
      return false;
    }
    for (const l of list.slice()) {
      if (l.once) this.removeListener(event, l.fn);
      l.fn.apply(this, args);
    }
    return true;
  }
  listenerCount(event: string | symbol): number {
    return this.listenersMap.get(event)?.length ?? 0;
  }
  listeners(event: string | symbol): Listener[] {
    return (this.listenersMap.get(event) ?? []).map((l) => l.fn);
  }
  eventNames(): (string | symbol)[] {
    return [...this.listenersMap.keys()];
  }
  setMaxListeners(n: number): this {
    return this;
  }
}

export function once(emitter: EventEmitter, event: string): Promise<any[]> {
  return new Promise((resolve) => emitter.once(event, (...args: any[]) => resolve(args)));
}

export default EventEmitter;
