//! Port of `apps/api/app/domains/finance/cost_catalog.py`: illustrative renovation unit-cost
//! ranges in integer cents. Planning values, NOT quotes; every work item generated from them
//! requires a professional quote.

use ss_core::string_enum;

string_enum! {
    /// A key of Python's `COSTS` dict.
    pub enum CostKey {
        SinkExtendFromExisting = "sink_extend_from_existing",
        SinkNewRunSlab = "sink_new_run_slab",
        PartitionRoomOfficeBuildout = "partition_room_office_buildout",
        PartitionRoomOpenShell = "partition_room_open_shell",
        ElectricalSubpanelAllowance = "electrical_subpanel_allowance",
        /// Broker reports a dedicated panel with enough amps: circuits only, smaller allowance
        /// (still unverified).
        ElectricalCircuitsReportedCapacity = "electrical_circuits_reported_capacity",
        HvacRezoneExisting = "hvac_rezone_existing",
        HvacNewDistribution = "hvac_new_distribution",
        Finishes = "finishes",
        PermitsFees = "permits_fees",
        DesignProfessional = "design_professional",
    }
}

/// `(unit, low, expected, high)`, in integer cents per unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitCost {
    pub unit: &'static str,
    pub low: i64,
    pub expected: i64,
    pub high: i64,
}

const fn uc(unit: &'static str, low: i64, expected: i64, high: i64) -> UnitCost {
    UnitCost {
        unit,
        low,
        expected,
        high,
    }
}

impl CostKey {
    /// `COSTS[key]`.
    pub const fn unit_cost(self) -> UnitCost {
        match self {
            CostKey::SinkExtendFromExisting => uc("sink", 250000, 450000, 800000),
            CostKey::SinkNewRunSlab => uc("sink", 800000, 1500000, 2500000),
            CostKey::PartitionRoomOfficeBuildout => uc("room", 200000, 400000, 700000),
            CostKey::PartitionRoomOpenShell => uc("room", 700000, 1100000, 1600000),
            CostKey::ElectricalSubpanelAllowance => uc("allowance", 0, 600000, 1800000),
            CostKey::ElectricalCircuitsReportedCapacity => uc("allowance", 0, 200000, 700000),
            CostKey::HvacRezoneExisting => uc("room", 100000, 250000, 500000),
            CostKey::HvacNewDistribution => uc("room", 500000, 900000, 1500000),
            CostKey::Finishes => uc("m2", 9000, 15000, 25000),
            CostKey::PermitsFees => uc("lot", 300000, 500000, 900000),
            CostKey::DesignProfessional => uc("lot", 500000, 900000, 1500000),
        }
    }
}

/// 15.00% of the renovation subtotal, in basis points.
pub const CONTINGENCY_RATE_BP: i64 = 1500;
/// Furniture/fixtures/equipment allowance per treatment room, in cents.
pub const FF_E_PER_TREATMENT_ROOM: i64 = 450000;
