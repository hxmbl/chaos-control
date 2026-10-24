import { useState } from "react";
import { Composer } from "./components/Composer";
import { Conversation } from "./components/Conversation";
import { Sidebar } from "./components/Sidebar";
import { Titlebar } from "./components/Titlebar";
import { channels, projects, quickLinks } from "./data/sample";

export default function App() {
  const project = projects[0];
  const [selectedId, setSelectedId] = useState(channels[0].id);
  const channel = channels.find((c) => c.id === selectedId) ?? channels[0];

  return (
    <div className="relative flex h-full flex-col">
      <Titlebar />

      <div className="relative min-h-0 flex-1">
        {/* Centre column. Mirrors the sidebar's width on the right when there is room, so content sits mid-window. */}
        <main className="absolute inset-0 flex flex-col pr-5 pl-[290px] min-[1380px]:pr-[290px]">
          <div className="min-h-0 flex-1 pb-32">
            <Conversation channel={channel} projectName={project.name} />
          </div>
          <div className="pointer-events-none absolute inset-x-0 bottom-6 flex justify-center pr-5 pl-[290px] min-[1380px]:pr-[290px]">
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
      </div>
    </div>
  );
}
