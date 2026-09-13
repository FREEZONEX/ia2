/** Display the shortcuts that the current platform actually accepts. */
export function shortcut(key: string): string {
  const mac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform)
  return mac ? `⌘${key}` : `Ctrl+${key}`
}
