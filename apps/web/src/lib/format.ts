// Money is integer minor units (cents) in every contract.
export const usd = (cents: number | null | undefined) =>
  cents == null ? "—" : (cents / 100).toLocaleString("en-US", { style: "currency", currency: "USD", maximumFractionDigits: 0 });

// Geometry is meters internally; display converts.
export const sqft = (sqm: number | null | undefined) => (sqm == null ? "—" : `${Math.round(sqm * 10.7639).toLocaleString()} sq ft`);

export const shortId = (id: string) => id.slice(0, 8);
