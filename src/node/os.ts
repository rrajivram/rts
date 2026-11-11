// node:os
const info = __rts_os_info();
export const EOL = "\n";
export function platform(): string {
  return info.platform;
}
export function homedir(): string {
  return info.homedir;
}
export function tmpdir(): string {
  return info.tmpdir;
}
export function hostname(): string {
  return info.hostname;
}
export function arch(): string {
  return info.arch;
}
export function type(): string {
  return info.platform === "darwin" ? "Darwin" : info.platform === "linux" ? "Linux" : info.platform;
}
export function cpus(): { model: string; speed: number }[] {
  const out: { model: string; speed: number }[] = [];
  for (let i = 0; i < info.cpus; i++) out.push({ model: "cpu", speed: 0 });
  return out;
}
export function availableParallelism(): number {
  return info.cpus;
}
export default { EOL, platform, homedir, tmpdir, hostname, arch, type, cpus, availableParallelism };
