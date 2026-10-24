/** True inside the Tauri webview (Tauri 2 injects this global). */
export const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** macOS, where the window uses an overlay titlebar with real traffic lights. */
export const isMac = typeof navigator !== "undefined" && /Mac/i.test(navigator.platform || navigator.userAgent);
