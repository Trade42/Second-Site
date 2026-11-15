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
  { href: "/design", label: "Layout", icon: PencilRuler },
  { href: "/studio", label: "Design Studio", icon: PencilRuler },
  { href: "/procurement", label: "Procurement", icon: ShoppingCart },
  { href: "/activity", label: "Activity & Communications", icon: Activity },
];

export function AppShell({ children }: { children: ReactNode }) {
  const path = usePathname();
  const studio = path.startsWith("/studio");
  return (
    <div className={studio ? "flex h-screen overflow-hidden" : "flex min-h-screen"}>
      <aside className={cn("shrink-0 overflow-y-auto border-r border-line bg-surface", studio ? "w-14 px-2 py-4 lg:w-56 lg:px-3 lg:py-5" : "w-56 px-3 py-5")}>
        <div className={cn("mb-6", studio ? "px-0 text-center lg:mb-8 lg:px-2 lg:text-left" : "mb-8 px-2")}>
          <div className="text-lg font-semibold">{studio ? <><span className="lg:hidden">S²</span><span className="hidden lg:inline">Second Site</span></> : "Second Site"}</div>
          <div className={cn("text-xs text-ink-muted", studio && "hidden lg:block")}>Your next location, from search to opening.</div>
        </div>
        <nav className="space-y-1">
          {NAV.map(({ href, label, icon: Icon }) => {
            const active = href === "/" ? path === "/" : path.startsWith(href);
            return (
              <Link
                key={href}
                href={href}
                title={label}
                className={cn(
                  "flex items-center gap-3 rounded-lg px-3 py-2 text-sm",
                  studio && "justify-center px-2 lg:justify-start lg:px-3",
                  active ? "bg-gold-soft font-medium text-ink ring-1 ring-gold/40" : "text-ink-muted hover:bg-sage-soft",
                )}
              >
                <Icon className="h-4 w-4 shrink-0" />
                <span className={studio ? "hidden lg:inline" : undefined}>{label}</span>
              </Link>
            );
          })}
        </nav>
      </aside>
      <main className={studio ? "flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden" : "min-w-0 flex-1 px-10 py-8"}>
        {studio ? (
          children
        ) : (
          <>
            <ProjectHeader />
            <div className="mt-8">{children}</div>
          </>
        )}
      </main>
    </div>
  );
}
