// node:url (file URLs only)
export function fileURLToPath(url: any): string {
  const s = String(url);
  return decodeURIComponent(s.startsWith("file://") ? s.slice(7) : s);
}
export function pathToFileURL(path: string): { href: string; pathname: string; toString(): string } {
  const href = "file://" + encodeURI(path);
  return { href, pathname: path, toString: () => href };
}
export default { fileURLToPath, pathToFileURL };
