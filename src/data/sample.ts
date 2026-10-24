// Hardcoded sample data for the static shell. Nothing here is persisted.
import type { IconifyIcon } from "@iconify/react";
import {
  claude,
  devin,
  github,
  githubCopilot,
  linear,
  mistral,
  openai,
  opencode,
  pi,
  vercel,
  zed,
} from "../icons/brands";

export type Project = {
  id: string;
  name: string;
  emoji: string;
};

export type Channel = {
  id: string;
  name: string;
  /** Optional link to a Linear issue. Issues never become channels on their own. */
  linearIssue?: string;
  unread?: boolean;
  /** The project-wide room every project has. */
  projectWide?: boolean;
};

export type QuickLink = {
  id: string;
  label: string;
  icon: IconifyIcon;
  href: string;
};

export const projects: Project[] = [{ id: "starpoint", name: "Starpoint", emoji: "⭐" }];

export const channels: Channel[] = [
  { id: "main", name: "main", projectWide: true },
  { id: "strm-335", name: "STRM-335", linearIssue: "STRM-335" },
  { id: "strm-327", name: "STRM-327", linearIssue: "STRM-327", unread: true },
];

export const quickLinks: QuickLink[] = [
  { id: "github", label: "GitHub", icon: github, href: "https://github.com" },
  { id: "linear", label: "Linear", icon: linear, href: "https://linear.app" },
  { id: "zed", label: "Zed", icon: zed, href: "https://zed.dev" },
  { id: "vercel", label: "Vercel", icon: vercel, href: "https://vercel.com" },
];

/** working = green, idle = blue, blocked = red. */
export type AgentStatus = "working" | "idle" | "blocked";

export type Agent = {
  id: string;
  name: string;
  icon: IconifyIcon;
  status: AgentStatus;
};

/** Order follows the wireframe's 4x2 grid, left to right, top row first. */
export const agents: Agent[] = [
  { id: "codex-plan", name: "Codex (planner)", icon: openai, status: "working" },
  { id: "claude-code", name: "Claude Code", icon: claude, status: "idle" },
  { id: "devin", name: "Devin", icon: devin, status: "working" },
  { id: "codex-review", name: "Codex (reviewer)", icon: openai, status: "working" },
  { id: "copilot", name: "GitHub Copilot", icon: githubCopilot, status: "working" },
  { id: "mistral", name: "Mistral Vibe", icon: mistral, status: "blocked" },
  { id: "opencode", name: "opencode", icon: opencode, status: "idle" },
  { id: "pi", name: "Pi", icon: pi, status: "blocked" },
];

export type Mention = {
  id: string;
  agentId: string;
  channelId: string;
  text: string;
};

export const mentions: Mention[] = [
  {
    id: "m1",
    agentId: "opencode",
    channelId: "main",
    text: "Hey @Hx. So what's the direction? I've mapped out two approaches for the sync layer and need you to pick one.",
  },
  {
    id: "m2",
    agentId: "opencode",
    channelId: "strm-335",
    text: "Hey @Hx. Wake up already, we have work to do and the release branch is waiting on your review.",
  },
  {
    id: "m3",
    agentId: "opencode",
    channelId: "strm-327",
    text: "Hey @Hx. everything you made is failing the new lint rules, so I've queued a fix-up PR for you to look at.",
  },
  {
    id: "m4",
    agentId: "opencode",
    channelId: "main",
    text: "Hey @Hx. Just got your code improved and benchmarked: cold start is down 38% on the M3.",
  },
];
