import { Hash } from "@phosphor-icons/react";
import type { Channel } from "../data/sample";

/** The channel's message area. Empty for now: no backend, no agents yet. */
export function Conversation({ channel, projectName }: { channel: Channel; projectName: string }) {
  return (
    <section aria-label={`#${channel.name}`} className="grid h-full place-items-center px-6">
      <div className="flex max-w-sm flex-col items-center gap-2 text-center">
        <span className="grid size-10 place-items-center rounded-full border border-line text-faint">
          <Hash size={18} />
        </span>
        <p className="text-[15px] font-semibold text-ink-soft">{channel.name}</p>
        <p className="text-[13px] leading-5 text-muted">
          {channel.projectWide
            ? `The project-wide room. Everyone on ${projectName}, human or AI, can talk here.`
            : channel.linearIssue
              ? `Linked to ${channel.linearIssue} on Linear. Nothing here yet.`
              : "Nothing here yet."}
        </p>
      </div>
    </section>
  );
}
