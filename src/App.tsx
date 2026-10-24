import { useState } from "react";
import { Composer } from "./components/Composer";
import { Conversation } from "./components/Conversation";
import { RightSidebar } from "./components/RightSidebar";
import { Sidebar } from "./components/Sidebar";
import { Titlebar } from "./components/Titlebar";
import { agents, channels, mentions, projects, quickLinks } from "./data/sample";

export default function App() {
  const project = projects[0];
  const [selectedId, setSelectedId] = useState(channels[0].id);
  const channel = channels.find((c) => c.id === selectedId) ?? channels[0];

  return (
    <div className="relative flex h-full flex-col">
      <Titlebar />

      <div className="relative min-h-0 flex-1">
        {/*
          Centre column. Below 1200px only the left sidebar shows. From 1200px the right sidebar
          appears and both sides reserve the wider panel's width, so search and composer stay mid-window.
        */}
        <main className="absolute inset-0 flex flex-col pr-5 pl-[290px] min-[1200px]:px-[344px]">
          <div className="min-h-0 flex-1 pb-32">
            <Conversation channel={channel} projectName={project.name} />
          </div>
          <div className="pointer-events-none absolute inset-x-0 bottom-6 flex justify-center pr-5 pl-[290px] min-[1200px]:px-[344px]">
            <div className="w-full max-w-[800px]">
              <Composer channelName={channel.name} />
            </div>
          </div>
        </main>

        {/* Floating, detached sidebar. */}
        <aside className="absolute left-3 w-[258px] top-[max(4px,calc(14.3vh-52px))] bottom-[max(12px,14.3vh)]">
          <Sidebar
            project={project}
            channels={channels}
            quickLinks={quickLinks}
            selectedId={selectedId}
            onSelect={setSelectedId}
          />
        </aside>

        {/* Floating, detached activity panel mirroring the left sidebar. Hidden below 1200px. */}
        <div className="absolute right-3 hidden w-[320px] top-[max(4px,calc(14.3vh-52px))] bottom-[max(12px,14.3vh)] min-[1200px]:block">
          <RightSidebar
            agents={agents}
            mentions={mentions}
            onOpenMention={(m) => setSelectedId(m.channelId)}
          />
        </div>
      </div>
    </div>
  );
}
