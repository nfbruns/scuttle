//! # Core-Guided Search Functionality

use rustsat::{
    encodings::{
        card::dbtotalizer::TotDb,
        nodedb::{NodeById, NodeId, NodeLike},
    },
    instances::{Cnf, ManageVars},
    solvers::{SolveIncremental, SolverResult},
    types::{Assignment, Lit, RsHashMap},
};

use crate::Termination;

use super::{Objective, SolverKernel};

#[derive(Clone, Copy)]
pub struct TotOutput {
    root: NodeId,
    oidx: usize,
    tot_weight: usize,
}

#[derive(Default)]
pub struct OllReformulation {
    /// Inactive literals, aka the reformulated objective
    pub inactives: RsHashMap<Lit, usize>,
    /// Mapping totalizer output assumption to totalizer data
    pub outputs: RsHashMap<Lit, TotOutput>,
    /// The constant offset derived by the reformulation
    pub offset: usize,
}

impl From<&Objective> for OllReformulation {
    fn from(value: &Objective) -> Self {
        match value {
            Objective::Weighted { lits, .. } => OllReformulation {
                inactives: lits.clone(),
                ..Default::default()
            },
            Objective::Unweighted { lits, .. } => OllReformulation {
                inactives: lits.iter().map(|l| (*l, 1)).collect(),
                ..Default::default()
            },
            Objective::Constant { .. } => OllReformulation {
                ..Default::default()
            },
        }
    }
}

struct CoreData {
    idx: usize,
    len: usize,
    weight: usize,
}

impl<VM, O, BCG> SolverKernel<VM, O, BCG>
where
    VM: ManageVars,
    O: SolveIncremental,
{
    /// OLL core-guided search over an objective. The implementation includes the following
    /// refinements:
    /// - Weight-aware core extraction
    /// - Core exhaustion
    pub fn oll(
        &mut self,
        mut reform: OllReformulation,
        tot_db: &mut TotDb,
    ) -> Result<(Assignment, OllReformulation), Termination> {
        self.log_routine_start("oll")?;

        // cores not yet reformulated (because of WCE)
        let mut unreform_cores = vec![];
        let mut core_lits = vec![];

        let mut assumps: Vec<_> = reform.inactives.iter().map(|(&l, _)| !l).collect();

        loop {
            // Build assumptions sorted by weight
            assumps.sort_unstable_by_key(|&l| -(reform.inactives[&!l] as isize));
            // Remove assumptions that turned active
            while reform.inactives[&!assumps[assumps.len() - 1]] == 0 {
                assumps.pop();
            }

            match self.solve_assumps(&assumps)? {
                SolverResult::Interrupted => panic!(),
                SolverResult::Sat => {
                    if unreform_cores.is_empty() {
                        let sol = self.oracle.solution(self.var_manager.max_var().unwrap())?;
                        // Cleanup: remove literals that turned active from inactives
                        reform.inactives.retain(|_, w| *w > 0);
                        self.log_routine_end()?;
                        return Ok((sol, reform));
                    }
                    // TODO: maybe get solution and do hardening
                    // Reformulate cores
                    let mut encs = Cnf::new();
                    for CoreData { idx, len, weight } in unreform_cores.drain(..) {
                        let root = tot_db.lit_tree(&core_lits[idx..idx + len]);
                        let olit = tot_db.define_pos(root, 1, &mut encs, &mut self.var_manager);
                        reform.inactives.insert(olit, weight);
                        reform.outputs.insert(
                            olit,
                            TotOutput {
                                root,
                                oidx: 1,
                                tot_weight: weight,
                            },
                        );
                        assumps.push(!olit);
                    }
                    self.oracle.add_cnf(encs)?;
                    core_lits.clear();
                }
                SolverResult::Unsat => {
                    let core = self.oracle.core()?;
                    debug_assert!(!core.is_empty());
                    let core_weight = core
                        .iter()
                        .fold(usize::MAX, |cw, l| std::cmp::min(cw, reform.inactives[l]));
                    reform.offset += core_weight;
                    // Extend tot if output in core
                    let mut encs = Cnf::new();
                    for olit in &core {
                        if let Some(inact_weight) = reform.inactives.get_mut(olit) {
                            *inact_weight -= core_weight;
                            if *inact_weight > 0 {
                                continue;
                            }
                        }
                        debug_assert!(reform.inactives.contains_key(olit));
                        if let Some(&TotOutput {
                            root,
                            oidx,
                            tot_weight,
                        }) = reform.outputs.get(olit)
                        {
                            if oidx + 1 >= tot_db[root].len() {
                                continue;
                            }
                            let new_olit =
                                tot_db.define_pos(root, oidx + 1, &mut encs, &mut self.var_manager);
                            reform.inactives.insert(new_olit, tot_weight);
                            reform.outputs.insert(
                                new_olit,
                                TotOutput {
                                    root,
                                    oidx: oidx + 1,
                                    tot_weight,
                                },
                            );
                            assumps.push(!new_olit);
                        }
                    }
                    self.oracle.add_cnf(encs)?;
                    if core.len() > 1 {
                        // Save core for reformulation
                        unreform_cores.push(CoreData {
                            idx: core_lits.len(),
                            len: core.len(),
                            weight: core_weight,
                        });
                        core_lits.extend(core);
                    }
                }
            }
        }
    }
}
