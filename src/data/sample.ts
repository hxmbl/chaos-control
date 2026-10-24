// Hardcoded sample data for the static shell. Nothing here is persisted.
import type { IconifyIcon } from "@iconify/react";
import { github, linear, vercel, zed } from "../icons/brands";

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
