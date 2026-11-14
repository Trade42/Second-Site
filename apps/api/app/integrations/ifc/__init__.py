"""IFC model reader for the IFC-Bench engineering benchmark (isolated from real property data).

Uses IfcOpenShell when installed (`uv sync --extra ifc`). Otherwise a minimal STEP-text reader
handles the small committed fixture so the extraction contract stays testable.
Observations produced here always carry provider='ifc_bench' and property_id=None.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path

MEP_TYPES = (
    "IFCPIPESEGMENT",
    "IFCDUCTSEGMENT",
    "IFCSANITARYTERMINAL",
    "IFCFLOWTERMINAL",
    "IFCELECTRICDISTRIBUTIONBOARD",
    "IFCAIRTERMINAL",
    "IFCPUMP",
    "IFCVALVE",
)


@dataclass(frozen=True)
class IfcElement:
    global_id: str
    ifc_type: str
    name: str | None
    properties: dict[str, float | str | bool]
    source_file: str


def ifcopenshell_available() -> bool:
    try:
        import ifcopenshell  # noqa: F401
    except ImportError:
        return False
    return True


def read_mep_elements(path: Path) -> list[IfcElement]:
    if ifcopenshell_available():
        return _read_with_ifcopenshell(path)
    return _read_step_text(path)


def _read_with_ifcopenshell(path: Path) -> list[IfcElement]:
    import ifcopenshell
    import ifcopenshell.util.element as eu

    model = ifcopenshell.open(str(path))
    out = []
    for t in MEP_TYPES:
        try:
            elems = model.by_type(t)
        except RuntimeError:
            continue  # type not in this schema
        for e in elems:
            props: dict[str, float | str | bool] = {}
            for pset, values in eu.get_psets(e).items():
                for k, v in values.items():
                    if k != "id" and isinstance(v, (int, float, str, bool)):
                        props[f"{pset}.{k}"] = v
            out.append(IfcElement(e.GlobalId, e.is_a().upper(), getattr(e, "Name", None), props, path.name))
    return out


_ENTITY = re.compile(r"^#(\d+)\s*=\s*([A-Z0-9_]+)\((.*)\);\s*$", re.M)
_STR = re.compile(r"'((?:[^']|'')*)'")


def _read_step_text(path: Path) -> list[IfcElement]:
    """Tiny fallback parser: elements, IFCPROPERTYSINGLEVALUE, IFCPROPERTYSET, IFCRELDEFINESBYPROPERTIES."""
    text = path.read_text()
    ents = {int(m.group(1)): (m.group(2), m.group(3)) for m in _ENTITY.finditer(text)}
    props: dict[int, tuple[str, float | str | bool]] = {}
    for eid, (t, args) in ents.items():
        if t == "IFCPROPERTYSINGLEVALUE":
            name = _STR.findall(args)[0]
            val_m = re.search(r"IFC[A-Z]+\(([^)]*)\)", args)
            raw = val_m.group(1).strip("'") if val_m else ""
            try:
                val: float | str | bool = float(raw)
            except ValueError:
                val = {".T.": True, ".F.": False}.get(raw, raw)
            props[eid] = (name, val)
    psets: dict[int, tuple[str, list[int]]] = {}
    for eid, (t, args) in ents.items():
        if t == "IFCPROPERTYSET":
            name = _STR.findall(args)[1] if len(_STR.findall(args)) > 1 else "Pset"
            refs = [int(x) for x in re.findall(r"#(\d+)", args.split("(", 1)[-1])]
            psets[eid] = (name, refs)
    elem_props: dict[int, dict[str, float | str | bool]] = {}
    for _eid, (t, args) in ents.items():
        if t == "IFCRELDEFINESBYPROPERTIES":
            refs = [int(x) for x in re.findall(r"#(\d+)", args)]
            pset_id = refs[-1]
            if pset_id in psets:
                pname, prop_ids = psets[pset_id]
                for target in refs[:-1]:
                    for pid in prop_ids:
                        if pid in props:
                            k, v = props[pid]
                            elem_props.setdefault(target, {})[f"{pname}.{k}"] = v
    out = []
    for eid, (t, args) in ents.items():
        if t in MEP_TYPES:
            strs = _STR.findall(args)
            out.append(
                IfcElement(
                    strs[0] if strs else str(eid),
                    t,
                    strs[1] if len(strs) > 1 else None,
                    elem_props.get(eid, {}),
                    path.name,
                )
            )
    return out
