import { useSyncExternalStore } from "react"

/** The workbench defaults to light, with one persisted preference shared by
 * same-origin IDE/HMI windows. The native host observes the applied .dark class. */
const STORAGE_KEY = "ia2.theme"
const LEGACY_STORAGE_KEY = "controlsoftware.theme"
type Theme = "light" | "dark"
const listeners = new Set<() => void>()

function applyTheme(theme: Theme) {
  if (typeof document === "undefined") return
  document.documentElement.classList.toggle("dark", theme === "dark")
  document.documentElement.style.colorScheme = theme
  for (const [name, content] of [
    ["theme-color", theme === "dark" ? "#151a19" : "#ffffff"],
    ["color-scheme", theme],
  ]) {
    let meta = document.head.querySelector<HTMLMetaElement>(`meta[name="${name}"]`)
    if (!meta) {
      meta = document.createElement("meta")
      meta.name = name
      document.head.append(meta)
    }
    meta.content = content
  }
  listeners.forEach((listener) => listener())
}

function storedTheme(): Theme {
  try {
    const stored = window.localStorage.getItem(STORAGE_KEY)
    if (stored !== null) return stored === "dark" ? "dark" : "light"
    const legacy = window.localStorage.getItem(LEGACY_STORAGE_KEY)
    if (legacy === "light" || legacy === "dark") {
      try {
        window.localStorage.setItem(STORAGE_KEY, legacy)
        window.localStorage.removeItem(LEGACY_STORAGE_KEY)
      } catch { /* Keep the readable preference even if migration cannot write. */ }
      return legacy
    }
  } catch { /* Storage is optional; the light theme still renders. */ }
  return "light"
}

function storageChanged(event: StorageEvent) {
  if (event.key !== STORAGE_KEY && event.key !== null) return
  try {
    if (event.storageArea !== window.localStorage) return
  } catch { return }
  // Removal/clear restores the default. Ignore malformed values, and never
  // write back on a storage event: other windows already share that storage.
  if (event.newValue === null) applyTheme("light")
  else if (event.newValue === "light" || event.newValue === "dark") applyTheme(event.newValue)
}

// Apply before first render, including browser chrome and native form controls.
if (typeof window !== "undefined") {
  applyTheme(storedTheme())
  window.addEventListener("storage", storageChanged)
  import.meta.hot?.dispose(() => window.removeEventListener("storage", storageChanged))
}

function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => listeners.delete(listener)
}
function snapshot(): Theme {
  if (typeof document === "undefined") return "light"
  return document.documentElement.classList.contains("dark") ? "dark" : "light"
}

export function setTheme(theme: Theme) {
  if (typeof document === "undefined") return
  applyTheme(theme)
  try { window.localStorage.setItem(STORAGE_KEY, theme) } catch { /* Optional preference. */ }
}

export function useDarkMode(): Theme {
  return useSyncExternalStore(subscribe, snapshot, () => "light")
}

export function useThemeToggle(): {
  theme: Theme
  setTheme: (theme: Theme) => void
  toggle: () => void
} {
  const theme = useDarkMode()
  return { theme, setTheme, toggle: () => setTheme(theme === "dark" ? "light" : "dark") }
}
