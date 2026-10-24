export class Lifetime {
  #controller = new AbortController();
  #latest = new WeakMap<object, object>();
  #pending = new Map<string, Promise<void>>();
  #attempted = new Map<string, number>();
  #cleanup = new Set<() => void>();

  get signal() {
    return this.#controller.signal;
  }
  get active() {
    return !this.signal.aborted;
  }

  latest(resource: object) {
    const version = {};
    this.#latest.set(resource, version);
    return () => this.active && this.#latest.get(resource) === version;
  }

  once(key: string, work: () => Promise<void>, interval = 0): Promise<void> {
    if (!this.active) return Promise.resolve();
    const pending = this.#pending.get(key);
    if (pending) return pending;
    if (Date.now() - (this.#attempted.get(key) ?? -Infinity) < interval)
      return Promise.resolve();
    this.#attempted.set(key, Date.now());
    const task = Promise.resolve()
      .then(() => (this.active ? work() : undefined))
      .finally(() => this.#pending.delete(key));
    this.#pending.set(key, task);
    return task;
  }

  every(
    ms: number,
    work: () => Promise<unknown>,
    immediate = false,
    visibility: EventTarget = document,
    visible = () => !document.hidden,
  ) {
    let running = false,
      stopped = false;
    const run = async () => {
      if (running || stopped || !this.active || !visible()) return;
      running = true;
      try {
        await work();
      } catch {
      } finally {
        running = false;
      }
    };
    const timer = setInterval(run, ms);
    visibility.addEventListener("visibilitychange", run);
    const stop = () => {
      stopped = true;
      clearInterval(timer);
      visibility.removeEventListener("visibilitychange", run);
      this.#cleanup.delete(stop);
    };
    this.#cleanup.add(stop);
    if (!this.active) stop();
    else if (immediate) queueMicrotask(run);
    return stop;
  }

  dispose() {
    this.#controller.abort();
    for (const stop of this.#cleanup) stop();
    this.#pending.clear();
    this.#attempted.clear();
  }
}
