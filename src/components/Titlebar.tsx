import { MagnifyingGlass } from "@phosphor-icons/react";
import { isMac, isTauri } from "../lib/platform";

/**
 * Window titlebar. On macOS the Tauri window uses an overlay titlebar, so the real
 * traffic lights sit in the top-left gap and this bar is the drag region.
 * The browser preview has no window chrome, so it shows stand-in lights there.
 */
export function Titlebar() {
  return (
    <header data-tauri-drag-region className="relative z-10 flex h-[52px] shrink-0 items-center justify-center px-3">
      {!isTauri && <PreviewTrafficLights />}
      {isTauri && isMac && <div aria-hidden className="absolute inset-y-0 left-0 w-20" data-tauri-drag-region />}

      <label className="group relative flex h-[30px] w-[min(396px,46vw)] items-center rounded-full border border-line bg-surface text-ink transition-colors focus-within:border-line-strong hover:border-line-strong">
        <MagnifyingGlass size={15} weight="bold" className="pointer-events-none absolute left-3 text-muted" />
        <input
          type="search"
          placeholder="Looking for something? Let's search."
          aria-label="Search"
          className="h-full w-full rounded-full bg-transparent px-8 text-center text-[13px] outline-none placeholder:text-muted"
        />
      </label>
    </header>
  );
}

/** Browser-preview stand-in for the macOS traffic lights. Never rendered inside Tauri. */
function PreviewTrafficLights() {
  return (
    <div aria-hidden className="absolute top-1/2 left-5 flex -translate-y-1/2 gap-2" title="Preview only">
      <span className="size-3 rounded-full bg-[#ff5f57] ring-1 ring-black/10" />
      <span className="size-3 rounded-full bg-[#febc2e] ring-1 ring-black/10" />
      <span className="size-3 rounded-full bg-[#28c840] ring-1 ring-black/10" />
    </div>
  );
}
