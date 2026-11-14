// Minimal shadcn-style primitives (hand-written; run `npx shadcn init` later to adopt the full kit).
import type { ButtonHTMLAttributes, ReactNode } from "react";

export function cn(...c: (string | false | null | undefined)[]) {
  return c.filter(Boolean).join(" ");
}

export function Card({ children, className, selected }: { children: ReactNode; className?: string; selected?: boolean }) {
  return (
    <div className={cn("rounded-xl border bg-surface p-5", selected ? "border-gold bg-gold-soft/40" : "border-line", className)}>
      {children}
    </div>
  );
}

export function Button({ className, variant = "primary", ...props }: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: "primary" | "ghost" }) {
  return (
    <button
      className={cn(
        "inline-flex items-center gap-2 rounded-lg px-4 py-2 text-sm font-medium transition disabled:opacity-50",
        variant === "primary" ? "bg-sage text-white hover:bg-ink" : "border border-line hover:bg-sage-soft",
        className,
      )}
      {...props}
    />
  );
}

export function Badge({ children, tone = "neutral" }: { children: ReactNode; tone?: "neutral" | "good" | "warn" | "bad" | "gold" }) {
  const tones = {
    neutral: "bg-sage-soft text-ink",
    good: "bg-sage text-white",
    warn: "bg-gold-soft text-warn",
    bad: "bg-danger/10 text-danger",
    gold: "bg-gold text-white",
  } as const;
  return <span className={cn("inline-block rounded-full px-2.5 py-0.5 text-xs font-medium", tones[tone])}>{children}</span>;
}
