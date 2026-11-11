// node:process
const p = process;
export const argv = p.argv;
export const env = p.env;
export const platform = p.platform;
export const exit = p.exit;
export const cwd = p.cwd;
export const nextTick = p.nextTick;
export const stdout = p.stdout;
export const stderr = p.stderr;
export default p;
