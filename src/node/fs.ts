// node:fs (synchronous API, plus `promises`). Strings only: no Buffers yet.

export class Stats {
  constructor(public size: number, public mtimeMs: number, private file: boolean, private dir: boolean) {}
  isFile(): boolean {
    return this.file;
  }
  isDirectory(): boolean {
    return this.dir;
  }
  isSymbolicLink(): boolean {
    return false;
  }
  get mtime(): Date {
    return new Date(this.mtimeMs);
  }
}

function encodingOf(options: any): any {
  return typeof options === "string" ? options : options?.encoding;
}

export function readFileSync(path: string, options?: any): string {
  encodingOf(options);
  return __rts_fs_read(String(path));
}
export function writeFileSync(path: string, data: any, options?: any): void {
  __rts_fs_write(String(path), String(data));
}
export function appendFileSync(path: string, data: any, options?: any): void {
  __rts_fs_append(String(path), String(data));
}
export function existsSync(path: string): boolean {
  return __rts_fs_exists(String(path));
}
export function mkdirSync(path: string, options?: any): void {
  __rts_fs_mkdir(String(path), options?.recursive === true);
}
export function readdirSync(path: string, options?: any): string[] {
  return __rts_fs_readdir(String(path));
}
export function statSync(path: string): Stats {
  const s = __rts_fs_stat(String(path));
  return new Stats(s.size, s.mtimeMs, s.file, s.directory);
}
export const lstatSync = statSync;
export function unlinkSync(path: string): void {
  __rts_fs_unlink(String(path));
}
export function rmSync(path: string, options?: any): void {
  __rts_fs_rm(String(path), options?.recursive === true, options?.force === true);
}
export function rmdirSync(path: string, options?: any): void {
  __rts_fs_rm(String(path), options?.recursive === true, false);
}
export function renameSync(from: string, to: string): void {
  __rts_fs_rename(String(from), String(to));
}
export function copyFileSync(from: string, to: string): void {
  __rts_fs_copy(String(from), String(to));
}
export function realpathSync(path: string): string {
  return __rts_fs_realpath(String(path));
}

function promisify0<T>(f: () => T): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    try {
      resolve(f());
    } catch (e) {
      reject(e);
    }
  });
}

export const promises = {
  readFile: (path: string, options?: any): Promise<string> => promisify0(() => readFileSync(path, options)),
  writeFile: (path: string, data: any, options?: any): Promise<void> => promisify0(() => writeFileSync(path, data, options)),
  appendFile: (path: string, data: any): Promise<void> => promisify0(() => appendFileSync(path, data)),
  mkdir: (path: string, options?: any): Promise<void> => promisify0(() => mkdirSync(path, options)),
  readdir: (path: string): Promise<string[]> => promisify0(() => readdirSync(path)),
  stat: (path: string): Promise<Stats> => promisify0(() => statSync(path)),
  unlink: (path: string): Promise<void> => promisify0(() => unlinkSync(path)),
  rm: (path: string, options?: any): Promise<void> => promisify0(() => rmSync(path, options)),
  rename: (a: string, b: string): Promise<void> => promisify0(() => renameSync(a, b)),
  copyFile: (a: string, b: string): Promise<void> => promisify0(() => copyFileSync(a, b)),
  access: (path: string): Promise<void> =>
    promisify0(() => {
      statSync(path);
    }),
};

export function readFile(path: string, options: any, cb?: any): void {
  const callback = typeof options === "function" ? options : cb;
  promises.readFile(path, typeof options === "function" ? undefined : options).then(
    (d) => callback(null, d),
    (e) => callback(e),
  );
}
export function writeFile(path: string, data: any, options: any, cb?: any): void {
  const callback = typeof options === "function" ? options : cb;
  promises.writeFile(path, data).then(
    () => callback(null),
    (e) => callback(e),
  );
}

export default {
  readFileSync, writeFileSync, appendFileSync, existsSync, mkdirSync, readdirSync, statSync, lstatSync,
  unlinkSync, rmSync, rmdirSync, renameSync, copyFileSync, realpathSync, promises, readFile, writeFile, Stats,
};
