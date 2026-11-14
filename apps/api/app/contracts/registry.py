"""Registry of every top-level contract exported to JSON Schema / TypeScript."""

from __future__ import annotations

from . import (
    api,
    approvals,
    common,
    communications,
    events,
    evidence,
    finance,
    geometry,
    infrastructure,
    permits,
    procurement,
    project,
    property,
    tasks,
)
from .common import Contract

_MODULES = [
    common,
    project,
    property,
    evidence,
    permits,
    infrastructure,
    finance,
    geometry,
    procurement,
    approvals,
    communications,
    tasks,
    events,
    api,
]


def _collect() -> dict[str, type[Contract]]:
    out: dict[str, type[Contract]] = {}
    for mod in _MODULES:
        for name, obj in vars(mod).items():
            if (
                isinstance(obj, type)
                and issubclass(obj, Contract)
                and obj is not Contract
                and obj.__module__ == mod.__name__
            ):
                out[name] = obj
    return dict(sorted(out.items()))


CONTRACT_REGISTRY: dict[str, type[Contract]] = _collect()
