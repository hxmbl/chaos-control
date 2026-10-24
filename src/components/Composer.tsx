import { useState } from "react";
import { ArrowUp, Plus } from "@phosphor-icons/react";

/** Floating message box. Static shell: sending only clears the draft. */
export function Composer({ channelName }: { channelName: string }) {
  const [draft, setDraft] = useState("");
  const canSend = draft.trim().length > 0;

  return (
    <form
      className="pointer-events-auto flex w-full flex-col rounded-[22px] border border-line bg-surface px-2 pt-2.5 pb-2 shadow-float transition-colors focus-within:border-line-strong"
      onSubmit={(e) => {
        e.preventDefault();
        setDraft("");
      }}
    >
      <textarea
        rows={1}
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            e.currentTarget.form?.requestSubmit();
          }
        }}
        placeholder={`Message #${channelName}`}
        aria-label={`Message #${channelName}`}
        className="field-sizing-content max-h-40 min-h-6 resize-none bg-transparent px-2 text-[15px] leading-6 text-ink outline-none select-text placeholder:text-muted"
      />
      <div className="mt-1.5 flex items-center justify-between">
        <button
          type="button"
          className="grid size-8 place-items-center rounded-full text-muted transition-colors hover:bg-hover hover:text-ink"
          aria-label="Attach"
          title="Attach"
        >
          <Plus size={18} />
        </button>
        <button
          type="submit"
          disabled={!canSend}
          className="grid size-8 place-items-center rounded-full bg-ink text-surface transition-opacity disabled:opacity-15"
          aria-label="Send"
          title="Send"
        >
          <ArrowUp size={16} weight="bold" />
        </button>
      </div>
    </form>
  );
}
