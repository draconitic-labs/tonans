//! The entitlement seam.
//!
//! **Sales are disabled in this build and there is no licensing integration.**
//! No key is parsed, no server is contacted, nothing is validated. What exists
//! here is a *seam*: one narrow trait, the evaluation source that ships today, and
//! a test double. If commercial gating is ever wired up, it plugs in behind
//! [`EntitlementSource`] and nothing else in the codebase moves.
//!
//! The seam is enforced where it matters — scenario count, sensitivity size, and
//! whether a report may be handed to a client — because a gate that exists but is
//! not checked is not a seam, it is a comment.

use std::fmt;

use crate::error::{Error, Result};
use crate::limits;

/// What the user is entitled to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// The shipped default: enough to evaluate the product honestly.
    Evaluation,
    /// One analyst, unlimited local work, internal-use exports.
    Analyst,
    /// A team, with the right to hand reports to a client.
    Commercial,
}

impl Tier {
    /// The tier's name, as shown in the UI and stamped on every report.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Tier::Evaluation => "Evaluation",
            Tier::Analyst => "Analyst",
            Tier::Commercial => "Commercial",
        }
    }
}

/// The concrete limits a tier carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entitlements {
    /// Which tier these came from.
    pub tier: Tier,
    /// User-created projects allowed. The bundled demonstration project does not
    /// count against this.
    pub max_user_projects: Option<u32>,
    /// Scenarios allowed per project.
    pub max_scenarios_per_project: Option<u32>,
    /// Sensitivity cells allowed in one study.
    pub max_sensitivity_cells: u32,
    /// Whether an exported report may be given to a third party.
    ///
    /// When false, the report carries a visible evaluation notice. The notice is
    /// rendered by [`crate::report`], not enforced by the filesystem: this is a
    /// licence term, and Tonans states it rather than pretending to police it.
    pub client_report_rights: bool,
}

impl Entitlements {
    /// The evaluation envelope. See `TERMS.md`.
    #[must_use]
    pub const fn evaluation() -> Self {
        Self {
            tier: Tier::Evaluation,
            max_user_projects: Some(1),
            max_scenarios_per_project: Some(3),
            max_sensitivity_cells: 25,
            client_report_rights: false,
        }
    }

    /// One analyst, unlimited local work.
    #[must_use]
    pub const fn analyst() -> Self {
        Self {
            tier: Tier::Analyst,
            max_user_projects: None,
            max_scenarios_per_project: None,
            max_sensitivity_cells: limits::MAX_SENSITIVITY_CELLS as u32,
            client_report_rights: false,
        }
    }

    /// A team, with client-facing report rights.
    #[must_use]
    pub const fn commercial() -> Self {
        Self {
            tier: Tier::Commercial,
            max_user_projects: None,
            max_scenarios_per_project: None,
            max_sensitivity_cells: limits::MAX_SENSITIVITY_CELLS as u32,
            client_report_rights: true,
        }
    }

    /// Refuse a project that has more scenarios than this tier allows.
    pub fn check_scenario_count(&self, count: usize) -> Result<()> {
        if let Some(max) = self.max_scenarios_per_project {
            if count > max as usize {
                return Err(Error::NotEntitled {
                    code: "entitlement.scenario_limit",
                    message: format!(
                        "this project has {count} scenarios. The {tier} tier runs up to {max}. \
                         Remove a scenario to continue.",
                        tier = self.tier.label()
                    ),
                });
            }
        }
        Ok(())
    }

    /// Refuse a sensitivity study larger than this tier allows.
    pub fn check_sensitivity_cells(&self, cells: usize) -> Result<()> {
        if cells > self.max_sensitivity_cells as usize {
            return Err(Error::NotEntitled {
                code: "entitlement.sensitivity_limit",
                message: format!(
                    "this study has {cells} cells. The {tier} tier runs up to {max}. Reduce the \
                     number of values on an axis.",
                    tier = self.tier.label(),
                    max = self.max_sensitivity_cells
                ),
            });
        }
        Ok(())
    }

    /// Refuse a project beyond this tier's project count.
    pub fn check_project_count(&self, existing: usize) -> Result<()> {
        if let Some(max) = self.max_user_projects {
            if existing >= max as usize {
                return Err(Error::NotEntitled {
                    code: "entitlement.project_limit",
                    message: format!(
                        "the {tier} tier holds {max} project. Open the existing project, or remove \
                         it from the workspace folder first.",
                        tier = self.tier.label()
                    ),
                });
            }
        }
        Ok(())
    }

    /// Refuse use of a non-demo project when the workspace already exceeds this
    /// tier's project allowance. This closes the copied-file path around creation
    /// checks while keeping list/open available so the user can recover their data.
    pub fn check_project_usage(&self, existing: usize) -> Result<()> {
        if let Some(max) = self.max_user_projects {
            if existing > max as usize {
                return Err(Error::NotEntitled {
                    code: "entitlement.project_limit",
                    message: format!(
                        "this workspace contains {existing} user projects, but the {tier} tier may \
                         run or modify {max}. Project files remain readable; move the extras out of \
                         the workspace folder to continue.",
                        tier = self.tier.label()
                    ),
                });
            }
        }
        Ok(())
    }
}

/// Where entitlements come from.
///
/// The one seam a future licence check would sit behind. It is deliberately
/// narrow: no key, no expiry, no device binding, no network. Anything that needed
/// those would introduce them here, and only here.
pub trait EntitlementSource: fmt::Debug + Send + Sync {
    /// The entitlements in force.
    fn entitlements(&self) -> Entitlements;
}

/// The source that ships today. Always evaluation; consults nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltInEvaluation;

impl EntitlementSource for BuiltInEvaluation {
    fn entitlements(&self) -> Entitlements {
        Entitlements::evaluation()
    }
}

/// A test double. Lets the suite exercise every tier without a licence system
/// existing, which is the point of having a seam at all.
#[derive(Debug, Clone, Copy)]
pub struct FixedEntitlements(pub Entitlements);

impl EntitlementSource for FixedEntitlements {
    fn entitlements(&self) -> Entitlements {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_build_is_evaluation_and_consults_nothing() {
        let entitlements = BuiltInEvaluation.entitlements();
        assert_eq!(entitlements.tier, Tier::Evaluation);
        assert_eq!(entitlements.max_scenarios_per_project, Some(3));
        assert_eq!(entitlements.max_user_projects, Some(1));
        assert!(!entitlements.client_report_rights);
    }

    #[test]
    fn evaluation_gates_are_enforced_not_merely_declared() {
        let entitlements = Entitlements::evaluation();
        assert!(entitlements.check_scenario_count(3).is_ok());

        let error = entitlements.check_scenario_count(4).unwrap_err();
        assert_eq!(error.code(), "entitlement.scenario_limit");
        assert_eq!(error.http_status(), 403);

        assert!(entitlements.check_sensitivity_cells(25).is_ok());
        assert_eq!(
            entitlements.check_sensitivity_cells(26).unwrap_err().code(),
            "entitlement.sensitivity_limit"
        );

        assert!(entitlements.check_project_count(0).is_ok());
        assert_eq!(
            entitlements.check_project_count(1).unwrap_err().code(),
            "entitlement.project_limit"
        );
        assert!(entitlements.check_project_usage(1).is_ok());
        assert_eq!(
            entitlements.check_project_usage(2).unwrap_err().code(),
            "entitlement.project_limit"
        );
    }

    #[test]
    fn paid_tiers_lift_the_gates_through_the_same_seam() {
        let analyst = FixedEntitlements(Entitlements::analyst());
        assert!(analyst.entitlements().check_scenario_count(32).is_ok());
        assert!(analyst.entitlements().check_project_count(500).is_ok());
        assert!(!analyst.entitlements().client_report_rights);

        let commercial = FixedEntitlements(Entitlements::commercial());
        assert!(commercial.entitlements().client_report_rights);
        assert!(commercial
            .entitlements()
            .check_sensitivity_cells(limits::MAX_SENSITIVITY_CELLS)
            .is_ok());
    }

    #[test]
    fn no_tier_may_exceed_the_hard_engine_limits() {
        // An entitlement can only ever be more restrictive than the compiled-in
        // envelope. A licence must never be able to buy its way past a safety bound.
        for entitlements in [
            Entitlements::evaluation(),
            Entitlements::analyst(),
            Entitlements::commercial(),
        ] {
            assert!(entitlements.max_sensitivity_cells as usize <= limits::MAX_SENSITIVITY_CELLS);
            if let Some(max) = entitlements.max_scenarios_per_project {
                assert!(max as usize <= limits::MAX_SCENARIOS);
            }
        }
    }
}
