"""Hard-coded text replies for Photon while no chat model is configured (SUPERVISOR_MODEL unset).

Temporary stand-in for the chat assistant. Every figure comes from the project's own records; nothing is invented,
and a missing figure is said to be unknown. Returns None when a text matches no topic, so the caller can fall back.
"""

from __future__ import annotations

from sqlalchemy.orm import Session

TOPICS = {
    "cheapest": ("cheap", "cost", "price", "afford", "expensive", "budget", "spend", "total"),
    "rent": ("rent", "monthly", "a month", "per month", "lease"),
    "best": ("best", "top", "recommend", "pick", "which one", "which place", "should i", "winner"),
    "list": ("options", "list", "all of them", "places", "spaces", "properties", "listings", "finalists"),
    "next": ("next", "what now", "what do i do", "todo", "to do", "waiting", "pending"),
    "call": ("call", "broker", "landlord", "phone", "tour", "visit"),
    "help": ("help", "what can you", "how does", "commands", "menu", "hi", "hello", "hey", "yo", "sup"),
    "thanks": ("thanks", "thank you", "ty", "thx", "appreciate"),
}

HELP = (
    "Here's what I can answer by text:\n"
    '- "cheapest": which place costs least to open and run\n'
    '- "rent": advertised rents for your top places\n'
    '- "best": your top pick and why\n'
    '- "status": a quick update\n'
    '- "revise: ...": save a design change\n'
    "Approvals and calls happen in the app."
)


def _money(minor: int | None) -> str:
    return "unknown" if minor is None else f"${minor / 100:,.0f}"


def _topic(text: str) -> str | None:
    words = f" {text.lower().strip(' .!?')} "
    for topic, keys in TOPICS.items():
        if any(f" {k} " in words or (len(k) > 3 and k in words) for k in keys):
            return topic
    return None


def _ranked(db: Session, project_id):
    from app.repositories import registry as repos

    rankings = repos.rankings.list(db, project_id=project_id)
    props = {p.property_id: p for p in repos.properties.list(db, project_id=project_id)}
    if not rankings:
        return [], {}
    rows = [(r, props.get(r.property_id)) for r in rankings[-1].ranked_properties if not r.disqualified]
    return [(r, p) for r, p in rows if p], rankings[-1].explanations


def _addr(p) -> str:
    """'6922 W HOLLYWOOD BLVD STE 302, LOS ANGELES, CA 90028' -> '6922 W Hollywood Blvd Ste 302, Los Angeles, CA 90028'."""
    import re

    return re.sub(
        r", ([A-Z]{2}) (\d{5})",
        lambda m: f", {m.group(1).upper()} {m.group(2)}",
        p.property_identity.normalized_address.title().replace(", Ca ", ", CA "),
    )


def canned_reply(db: Session, project_id, text: str, web: str) -> str | None:
    topic = _topic(text)
    if topic is None:
        return None
    if topic == "help":
        return HELP
    if topic == "thanks":
        return 'Anytime. Text "help" to see what I can answer.'
    if topic == "call":
        return (
            "I don't call anyone from a text. Calls to a broker or landlord need your approval in the app first: "
            f"{web}/activity"
        )
    if topic == "next":
        from app.domains.communications.inbound import _status_text

        return _status_text(db, project_id, web)

    rows, why = _ranked(db, project_id)
    if not rows:
        return f"There's no ranking yet. Start a comparison in the app: {web}"

    if topic == "cheapest":
        priced = [(r, p) for r, p in rows if r.long_term_cost is not None]
        if not priced:
            return "None of your places has a full cost estimate yet, so I can't say which is cheapest."
        r, p = min(priced, key=lambda rp: rp[0].long_term_cost)
        if len(priced) == 1:
            return (
                f"Only one place has a full estimate so far: {_addr(p)}, about {_money(r.long_term_cost)} "
                "over the horizon (opening plus rent). Compare more places in the app to see which is cheapest."
            )
        return (
            f"Cheapest over the full horizon: {_addr(p)}, about {_money(r.long_term_cost)} "
            f"(opening plus rent). That's an estimate; anything unknown isn't included."
        )
    if topic == "rent":
        lines = []
        for r, p in rows[:3]:
            rent = p.commercial_listing.asking_rent_monthly if p.commercial_listing else None
            lines.append(f"#{r.rank} {_addr(p)}: {_money(rent)}/mo advertised")
        return "Advertised rents (listing figures, not confirmed):\n" + "\n".join(lines)
    if topic == "best":
        r, p = rows[0]
        reason = why.get(str(r.property_id), "")
        return f"Your top pick is {_addr(p)}. {reason}".strip()[:600]
    # list
    lines = [f"#{r.rank} {_addr(p)}: {_money(r.long_term_cost)} over the horizon" for r, p in rows[:5]]
    return "Your ranked places:\n" + "\n".join(lines)
