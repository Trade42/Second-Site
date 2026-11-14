"""IFC extraction path, isolated from real property data (provider='ifc_bench'-style, property_id=None)."""

from __future__ import annotations

import pytest

from app.integrations.ifc import ifcopenshell_available, read_mep_elements

from .conftest import FIXTURES


def test_mep_extraction_from_fixture():
    els = {e.ifc_type: e for e in read_mep_elements(FIXTURES / "ifc" / "mini_mep.ifc")}
    pipe = els["IFCPIPESEGMENT"]
    assert pipe.properties["Pset_PipeSegmentTypeCommon.NominalDiameter"] == pytest.approx(0.0508)
    assert els["IFCDUCTSEGMENT"].properties["Pset_DuctSegmentOccurrence.NominalAirFlowRate"] == pytest.approx(
        0.236
    )
    assert els["IFCELECTRICDISTRIBUTIONBOARD"].properties["Pset_ElectricalDeviceCommon.RatedCurrent"] == 200
    assert "IFCSANITARYTERMINAL" in els
    assert all(e.source_file == "mini_mep.ifc" for e in els.values())  # provenance preserved


@pytest.mark.ifc
def test_ifcopenshell_backend_if_installed():
    if not ifcopenshell_available():
        pytest.skip("ifcopenshell not installed (uv sync --extra ifc)")
    assert read_mep_elements(FIXTURES / "ifc" / "mini_mep.ifc")
