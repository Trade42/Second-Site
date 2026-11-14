"""Export canonical contracts: OpenAPI (for the TS client) + per-contract JSON Schema.

Usage: uv run python -m app.contracts.export <output_dir>   (scripts/generate-contracts.sh wraps this)
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

from pydantic.json_schema import models_json_schema

from .common import SCHEMA_VERSION
from .registry import CONTRACT_REGISTRY


def all_contract_schemas() -> dict[str, dict]:
    _, top = models_json_schema(
        [(m, "serialization") for m in CONTRACT_REGISTRY.values()],
        ref_template="#/components/schemas/{model}",
    )
    return top.get("$defs", {})


def _dump(obj: dict) -> str:
    return json.dumps(obj, indent=2, sort_keys=True) + "\n"


def main(out_dir: str) -> None:
    from app.main import app  # local import: builds routes

    out = Path(out_dir)
    (out / "schemas").mkdir(parents=True, exist_ok=True)
    (out / "openapi.json").write_text(_dump(app.openapi()))
    for name, model in CONTRACT_REGISTRY.items():
        schema = model.model_json_schema(ref_template="#/$defs/{model}")
        schema["$id"] = f"https://second-site.dev/contracts/{SCHEMA_VERSION}/{name}.json"
        (out / "schemas" / f"{name}.json").write_text(_dump(schema))
    (out / "schemas" / "index.json").write_text(
        _dump({"schema_version": SCHEMA_VERSION, "contracts": sorted(CONTRACT_REGISTRY)})
    )


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "../../packages/contracts")
