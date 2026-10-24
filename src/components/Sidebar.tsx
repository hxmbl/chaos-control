import { Archive, CaretDown, CaretRight, GearSix, Hash, Plus } from "@phosphor-icons/react";
import { Icon } from "@iconify/react";
import type { Channel, Project, QuickLink } from "../data/sample";
import { linear } from "../icons/brands";

type SidebarProps = {
  project: Project;
  channels: Channel[];
  quickLinks: QuickLink[];
  selectedId: string;
  onSelect: (id: string) => void;
};

/** Thin-outlined pill shared by every row in the sidebar. */
const pill =
  "flex w-full items-center rounded-full border border-line bg-surface text-ink transition-colors " +
  "hover:border-line-strong hover:bg-hover";

export function Sidebar({ project, channels, quickLinks, selectedId, onSelect }: SidebarProps) {
  return (
    <nav
      aria-label="Project"
      className="flex h-full w-full flex-col gap-1.5 rounded-[26px] border border-line bg-surface p-1.5 shadow-float"
    >
      <ProjectPicker project={project} />

      {/* Channels */}
      <div className="flex min-h-[232px] shrink basis-[52%] flex-col rounded-[21px] border border-line bg-sunken p-1.5">
        <ul className="flex min-h-0 flex-col gap-1.5 overflow-y-auto" role="list">
          {channels.map((channel) => (
            <li key={channel.id}>
              <ChannelPill
                channel={channel}
                selected={channel.id === selectedId}
                onSelect={() => onSelect(channel.id)}
              />
            </li>
          ))}
          <li>
            <button type="button" className={`${pill} h-9 justify-center text-ink-soft`} aria-label="New channel">
              <Plus size={18} weight="regular" />
            </button>
          </li>
        </ul>

        <div className="min-h-3 flex-1" />

        <button type="button" className={`${pill} h-9 gap-2.5 px-3 text-[15px]`}>
          <Archive size={18} className="text-ink-soft" />
          <span className="flex-1 text-left">Archived</span>
          <CaretRight size={16} className="text-muted" />
        </button>
      </div>

      {/* Quick links */}
      <div className="grid grid-cols-4 gap-1.5 rounded-[21px] border border-line bg-sunken p-1.5">
        {quickLinks.map((link) => (
          <a
            key={link.id}
            href={link.href}
            target="_blank"
            rel="noreferrer"
            title={link.label}
            aria-label={link.label}
            className={`${pill} h-10 justify-center text-ink-soft hover:text-ink`}
            draggable={false}
          >
            <Icon icon={link.icon} width={19} height={19} />
          </a>
        ))}
      </div>

      <div className="flex-1" />

      <button type="button" className={`${pill} h-11 gap-2.5 px-3.5 text-[15px]`}>
        <GearSix size={19} className="text-ink-soft" />
        <span>Settings</span>
      </button>
    </nav>
  );
}

function ProjectPicker({ project }: { project: Project }) {
  return (
    <button
      type="button"
      className={`${pill} h-11 gap-2 px-3.5 text-[15px] font-semibold`}
      aria-haspopup="listbox"
      title="Switch project"
    >
      <span aria-hidden className="text-base leading-none">
        {project.emoji}
      </span>
      <span className="flex-1 truncate text-left">{project.name}</span>
      <CaretDown size={16} className="text-muted" />
    </button>
  );
}

function ChannelPill({ channel, selected, onSelect }: { channel: Channel; selected: boolean; onSelect: () => void }) {
  return (
    <button
      type="button"
      onClick={onSelect}
      aria-current={selected ? "page" : undefined}
      className={
        `${pill} h-9 gap-2 pl-3 pr-2 text-[15px] ` +
        (selected ? "!border-line-strong !bg-selected font-semibold" : channel.unread ? "font-semibold" : "")
      }
    >
      <Hash size={17} weight={selected ? "bold" : "regular"} className={selected ? "text-ink" : "text-ink-soft"} />
      <span className="min-w-0 flex-1 truncate text-left">{channel.name}</span>
      {channel.unread && (
        <span className="size-1.5 shrink-0 rounded-full bg-accent" role="status" aria-label="Unread messages" />
      )}
      {channel.linearIssue && (
        <span
          className="grid size-5 shrink-0 place-items-center text-muted"
          title={`Linked to Linear issue ${channel.linearIssue}`}
          aria-label={`Linked to Linear issue ${channel.linearIssue}`}
        >
          <Icon icon={linear} width={15} height={15} />
        </span>
      )}
    </button>
  );
}
