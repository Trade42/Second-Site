"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { Activity, Building2, LayoutDashboard, PencilRuler, Search, ShoppingCart } from "lucide-react";
import type { ReactNode } from "react";
import { cn } from "@/components/ui/primitives";
import { ProjectHeader } from "@/components/ProjectHeader";

const NAV = [
  { href: "/", label: "Overview", icon: LayoutDashboard },
  { href: "/properties", label: "Explore Properties", icon: Search },
  { href: "/intelligence", label: "Property Intelligence", icon: Building2 },
  { href: "/design", label: "Design Studio", icon: PencilRuler },
  { href: "/procurement", label: "Procurement", icon: ShoppingCart },
  { href: "/activity", label: "Activity & Communications", icon: Activity },
];

export function AppShell({ children }: { children: ReactNode }) {
  const path = usePathname();
  return (
    <div className="flex min-h-screen">
      <aside className="w-64 shrink-0 border-r border-line bg-surface px-4 py-6">
        <div className="mb-8 px-2">
          <div className="text-lg font-semibold">Second Site</div>
          <div className="text-xs text-ink-muted">Your next location, from search to opening.</div>
        </div>
        <nav className="space-y-1">
          {NAV.map(({ href, label, icon: Icon }) => {
            const active = href === "/" ? path === "/" : path.startsWith(href);
            return (
              <Link
                key={href}
                href={href}
                className={cn(
                  "flex items-center gap-3 rounded-lg px-3 py-2 text-sm",
                  active ? "bg-gold-soft font-medium text-ink ring-1 ring-gold/40" : "text-ink-muted hover:bg-sage-soft",
                )}
              >
                <Icon className="h-4 w-4" /> {label}
              </Link>
            );
          })}
        </nav>
      </aside>
      <main className="flex-1 px-10 py-8">
        <ProjectHeader />
        <div className="mt-8">{children}</div>
      </main>
    </div>
  );
}
