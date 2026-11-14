import type { CostScenario } from "@second-site/contracts";
import { Card } from "@/components/ui/primitives";
import { usd } from "@/lib/format";

export function CostSummary({ s, title }: { s: CostScenario; title: string }) {
  return (
    <Card>
      <div className="text-sm font-medium">{title}</div>
      <div className="mt-2 text-2xl font-semibold">{usd(s.long_term_cost)}</div>
      <div className="text-xs text-ink-muted">{s.comparison_horizon_months}-month total (deposits excluded)</div>
      <dl className="mt-4 space-y-1 text-sm">
        <div className="flex justify-between"><dt>Opening (expected)</dt><dd>{usd(s.opening_cost_expected)}</dd></div>
        <div className="flex justify-between text-ink-muted"><dt>Range</dt><dd>{usd(s.opening_cost_low)} – {usd(s.opening_cost_high)}</dd></div>
        <div className="flex justify-between"><dt>Monthly occupancy</dt><dd>{usd(s.monthly_operating_cost)}</dd></div>
        <div className="flex justify-between"><dt>Upfront cash</dt><dd>{usd(s.upfront_cash_expected)}</dd></div>
      </dl>
    </Card>
  );
}
