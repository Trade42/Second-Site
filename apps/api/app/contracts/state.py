"""Generic guarded state machine helper used by approvals, calls, handoffs and tasks."""

from __future__ import annotations

from collections.abc import Mapping
from enum import StrEnum
from typing import TypeVar

S = TypeVar("S", bound=StrEnum)


class InvalidStateTransition(ValueError):
    def __init__(self, machine: str, current: str, target: str):
        super().__init__(f"{machine}: transition {current} -> {target} is not allowed")
        self.machine, self.current, self.target = machine, current, target


def check_transition(machine: str, table: Mapping[S, frozenset[S]], current: S, target: S) -> S:
    if target not in table.get(current, frozenset()):
        raise InvalidStateTransition(machine, str(current), str(target))
    return target
