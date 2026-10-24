import { useId, useState } from "react";
import { CaretUp, Sparkle } from "@phosphor-icons/react";
import { Icon } from "@iconify/react";
import type { Agent, AgentStatus, Mention } from "../data/sample";

type RightSidebarProps = {
  agents: Agent[];
  mentions: Mention[];
  onOpenMention?: (mention: Mention) => void;
};

/** Thin-outlined pill shared with the left sidebar. */
const pill =
  "flex w-full items-center rounded-full border border-line bg-surface text-ink transition-colors " +
  "hover:border-line-strong hover:bg-hover";

/** Floating, detached activity panel: catch-up summary, mentions, agent status. */
export function RightSidebar({ agents, mentions, onOpenMention }: RightSidebarProps) {
  const byId = new Map(agents.map((a) => [a.id, a]));

  return (
    <aside
      aria-label="Activity"
      className="flex h-full w-full flex-col gap-1.5 rounded-[26px] border border-line bg-surface p-1.5 shadow-float"
    >
      <CatchUp />
      <Mentions mentions={mentions} agentsById={byId} onOpen={onOpenMention} />
      <AgentStatusGrid agents={agents} />
      <div className="flex-1" />
    </aside>
  );
}

/* ----------------------------------------------------------------- catch-up */

/** Placeholder line widths, eyeballed from the wireframe. */
const SKELETON = [82, 90, 76, 97, 64, 74, 88, 79, 91, 52, 75];

function CatchUp() {
  return (
    <section
      aria-label="Catch-up"
      aria-busy="true"
      className="flex min-h-[148px] shrink flex-col overflow-hidden rounded-[21px] border border-line bg-sunken px-3.5 pt-3 pb-3"
    >
      <div className="flex items-start gap-2.5">
        <Sparkle size={19} weight="regular" className="mt-px shrink-0 text-ink-soft catchup-sparkle" aria-hidden />
        <p className="text-[15px] leading-5 font-medium text-ink">
          Summarizing what happened while you were gone…
        </p>
      </div>

      {/* Blurred, shimmering stand-in for the summary text. Purely decorative. */}
      <div aria-hidden className="relative mt-3 min-h-0 flex-1 overflow-hidden">
        <div className="flex flex-col gap-[6px] px-0.5 pt-0.5">
          {SKELETON.map((w, i) => (
            <span
              key={i}
              className="skeleton-line h-[13px] shrink-0 rounded-full"
              style={{ width: `${w}%`, animationDelay: `${i * 70}ms` }}
            />
          ))}
        </div>
        {/* Fade the overflow so a short window cuts off softly. */}
        <div className="pointer-events-none absolute inset-x-0 bottom-0 h-6 bg-gradient-to-t from-sunken to-transparent" />
      </div>
    </section>
  );
}

/* ----------------------------------------------------------------- mentions */

function Mentions({
  mentions,
  agentsById,
  onOpen,
}: {
  mentions: Mention[];
  agentsById: Map<string, Agent>;
  onOpen?: (mention: Mention) => void;
}) {
  const [open, setOpen] = useState(true);
  const listId = useId();
  const count = mentions.length;

  return (
    <section aria-label="Mentions" className="flex shrink-0 flex-col gap-1.5 rounded-[21px] border border-line bg-sunken p-1.5">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        aria-controls={listId}
        title={open ? "Collapse mentions" : "Expand mentions"}
        className={`${pill} h-9 justify-center gap-2 text-ink-soft hover:text-ink`}
      >
        {!open && (
          <span className="text-[13px] font-medium">
            {count} {count === 1 ? "mention" : "mentions"}
          </span>
        )}
        <CaretUp
          size={17}
          weight="regular"
          className={`transition-transform duration-200 motion-reduce:transition-none ${open ? "" : "rotate-180"}`}
        />
        <span className="sr-only">{open ? "Collapse mentions" : `Show ${count} mentions`}</span>
      </button>

      {open && (
        <ul id={listId} role="list" className="flex flex-col gap-1.5">
          {mentions.map((m) => {
            const agent = agentsById.get(m.agentId);
            return (
              <li key={m.id}>
                <button
                  type="button"
                  onClick={() => onOpen?.(m)}
                  title={`${agent?.name ?? "Agent"}: ${m.text}`}
                  className={`${pill} h-9 gap-2 pr-3 pl-2.5 text-left text-[13.5px]`}
                >
                  {agent && (
                    <span className="grid size-5 shrink-0 place-items-center text-ink" aria-hidden>
                      <Icon icon={agent.icon} width={16} height={16} />
                    </span>
                  )}
                  <span className="sr-only">{agent?.name ?? "Agent"}: </span>
                  <span className="min-w-0 flex-1 truncate">{m.text}</span>
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}

/* ------------------------------------------------------------- agent status */

const STATUS_LABEL: Record<AgentStatus, string> = {
  working: "Working",
  idle: "Idle",
  blocked: "Blocked",
};

const STATUS_GLOW: Record<AgentStatus, string> = {
  working: "glow-working",
  idle: "glow-idle",
  blocked: "glow-blocked",
};

function AgentStatusGrid({ agents }: { agents: Agent[] }) {
  return (
    <section
      aria-labelledby="agent-status-heading"
      className="flex shrink-0 flex-col gap-2 rounded-[21px] border border-line bg-sunken px-1.5 pt-2.5 pb-2.5"
    >
      <h2 id="agent-status-heading" className="px-2 text-[13px] font-semibold text-ink-soft">
        Agent Status
      </h2>
      <ul role="list" className="grid grid-cols-4 gap-x-1.5 gap-y-2.5">
        {agents.map((agent) => {
          const label = `${agent.name}: ${STATUS_LABEL[agent.status]}`;
          return (
            <li key={agent.id}>
              <button
                type="button"
                title={label}
                aria-label={label}
                className={`${pill} agent-pill ${STATUS_GLOW[agent.status]} h-11 justify-center text-ink`}
              >
                <Icon icon={agent.icon} width={19} height={19} />
              </button>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
