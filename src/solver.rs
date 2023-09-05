//! Core solver functionality shared between different algorithms

use std::ops::{Not, Range};

use rustsat::{
    encodings::{card, pb},
    instances::{Cnf, ManageVars, MultiOptInstance},
    solvers::{ControlSignal, FlipLit, PhaseLit, SolveIncremental, SolverResult, Terminate},
    types::{Assignment, Clause, Lit, LitIter, RsHashMap, TernaryVal, Var, WLitIter},
    var,
};

use crate::{
    options::EnumOptions,
    types::{NonDomPoint, ParetoFront},
    Limits, Options, Phase, Stats, Termination, WriteSolverLog,
};

pub mod lowerbounding;
pub mod pminimal;

/// Solving interface for each algorithm
pub trait Solve: KernelFunctions {
    /// Solves the instance under given limits. If not fully solved, errors an
    /// early termination reason.
    fn solve(&mut self, limits: Limits) -> Result<(), Termination>;
}

/// Shared functionality provided by the [`SolverKernel`]
pub trait KernelFunctions {
    /// Gets the Pareto front discovered so far
    fn pareto_front(&self) -> ParetoFront;
    /// Gets tracked statistics from the solver
    fn stats(&self) -> Stats;
    /// Attaches a logger to the solver
    fn attach_logger<L: WriteSolverLog + 'static>(&mut self, logger: L);
    /// Detaches a logger from the solver
    fn detach_logger(&mut self) -> Option<Box<dyn WriteSolverLog>>;
    /// Attaches a terminator callback. Only one callback can be attached at a time.
    fn attach_terminator(&mut self, term_cb: fn() -> ControlSignal);
    /// Detaches the termination callback
    fn detach_terminator(&mut self);
}

/// Kernel struct shared between all solvers
struct SolverKernel<VM, O, BCG> {
    /// The SAT solver backend
    oracle: O,
    /// The variable manager keeping track of variables
    var_manager: VM,
    /// Objective literal data
    obj_lit_data: RsHashMap<Lit, ObjLitData>,
    /// The maximum variable of the original encoding after introducing blocking
    /// variables
    max_orig_var: Var,
    /// The objectives
    objs: Vec<Objective>,
    /// The Pareto front discovered so far
    pareto_front: ParetoFront,
    /// Generator of blocking clauses
    block_clause_gen: BCG,
    /// Configuration options
    opts: Options,
    /// Running statistics
    stats: Stats,
    /// Limits for the current solving run
    lims: Limits,
    /// Logger to log with
    logger: Option<Box<dyn WriteSolverLog>>,
    /// Termination callback
    term_cb: Option<fn() -> ControlSignal>,
    // TODO: clock stack
}

impl<VM, O, BCG> SolverKernel<VM, O, BCG>
where
    VM: ManageVars,
    O: SolveIncremental,
    BCG: FnMut(Assignment) -> Clause,
{
    pub fn new(
        inst: MultiOptInstance<VM>,
        mut oracle: O,
        bcg: BCG,
        opts: Options,
    ) -> Result<Self, Termination> {
        let (constr, objs) = inst.decompose();
        let (cnf, mut var_manager) = constr.as_cnf();
        let stats = Stats {
            n_objs: objs.len(),
            n_orig_clauses: cnf.n_clauses(),
            ..Default::default()
        };
        // Add objectives to solver
        let mut obj_cnf = Cnf::new();
        let mut blits = RsHashMap::default();
        let objs: Vec<_> = objs
            .into_iter()
            .map(|obj| absorb_objective(obj, &mut obj_cnf, &mut blits, &mut var_manager))
            .collect();
        // Record objective literal occurrences
        let mut obj_lit_data: RsHashMap<_, ObjLitData> = RsHashMap::default();
        for (idx, obj) in objs.iter().enumerate() {
            match obj {
                Objective::Weighted { lits, .. } => {
                    for (&olit, _) in lits {
                        match obj_lit_data.get_mut(&olit) {
                            Some(data) => data.objs.push(idx),
                            None => {
                                obj_lit_data.insert(olit, ObjLitData { objs: vec![idx] });
                            }
                        }
                    }
                }
                Objective::Unweighted { lits, .. } => {
                    for &olit in lits {
                        match obj_lit_data.get_mut(&olit) {
                            Some(data) => data.objs.push(idx),
                            None => {
                                obj_lit_data.insert(olit, ObjLitData { objs: vec![idx] });
                            }
                        }
                    }
                }
                Objective::Constant { .. } => (),
            }
        }
        // Add hard clauses and relaxed soft clauses to oracle
        if var_manager.max_var().is_none() {
            return Err(Termination::NoVars);
        }
        oracle.reserve(var_manager.max_var().unwrap())?;
        oracle.add_cnf(cnf)?;
        oracle.add_cnf(obj_cnf)?;
        let max_orig_var = var_manager.max_var().unwrap();
        Ok(Self {
            oracle,
            var_manager,
            obj_lit_data,
            max_orig_var,
            objs,
            pareto_front: ParetoFront::new(),
            block_clause_gen: bcg,
            opts,
            stats,
            lims: Limits::none(),
            logger: None,
            term_cb: None,
        })
    }
}

impl<VM, O, BCG> SolverKernel<VM, O, BCG> {
    fn start_solving(&mut self, limits: Limits) {
        self.stats.n_solve_calls += 1;
        self.lims = limits;
    }

    fn attach_logger<L: WriteSolverLog + 'static>(&mut self, logger: L) {
        self.logger = Some(Box::new(logger));
    }

    fn detach_logger(&mut self) -> Option<Box<dyn WriteSolverLog>> {
        self.logger.take()
    }

    fn detach_terminator(&mut self) {
        self.term_cb = None
    }

    /// Converts an internal cost vector to an external one. Internal cost is
    /// purely the encoding output while external cost takes an offset and
    /// multiplier into account.
    fn externalize_internal_costs(&self, costs: Vec<usize>) -> Vec<isize> {
        debug_assert_eq!(costs.len(), self.stats.n_objs);
        costs
            .into_iter()
            .enumerate()
            .map(|(idx, cst)| match self.objs[idx] {
                Objective::Weighted { offset, .. } => {
                    let signed_cst: isize = cst.try_into().expect("cost exceeds `isize`");
                    signed_cst + offset
                }
                Objective::Unweighted {
                    offset,
                    unit_weight,
                    ..
                } => {
                    let signed_mult_cost: isize = (cst * unit_weight)
                        .try_into()
                        .expect("multiplied cost exceeds `isize`");
                    signed_mult_cost + offset
                }
                Objective::Constant { offset } => {
                    debug_assert_eq!(cst, 0);
                    offset
                }
            })
            .collect()
    }

    /// Blocks the current Pareto-MCS by blocking all blocking variables that are set
    fn block_pareto_mcs(&self, sol: Assignment) -> Clause {
        let mut blocking_clause = Clause::new();
        self.objs.iter().for_each(|oe| {
            oe.iter().for_each(|(l, _)| {
                if sol.lit_value(l) == TernaryVal::True {
                    blocking_clause.add(-l)
                }
            })
        });
        blocking_clause
            .normalize()
            .expect("Tautological blocking clause")
    }

    /// Checks the termination callback and terminates if appropriate
    fn check_terminator(&mut self) -> Result<(), Termination> {
        if let Some(cb) = &mut self.term_cb {
            if cb() == ControlSignal::Terminate {
                return Err(Termination::Callback);
            }
        }
        Ok(())
    }

    /// Logs a cost point candidate. Can error a termination if the candidates limit is reached.
    fn log_candidate(&mut self, costs: &Vec<usize>, phase: Phase) -> Result<(), Termination> {
        debug_assert_eq!(costs.len(), self.stats.n_objs);
        self.stats.n_candidates += 1;
        // Dispatch to logger
        if let Some(logger) = &mut self.logger {
            logger.log_candidate(costs, phase)?;
        }
        // Update limit and check termination
        if let Some(candidates) = &mut self.lims.candidates {
            *candidates -= 1;
            if *candidates == 0 {
                return Err(Termination::CandidatesLimit);
            }
        }
        Ok(())
    }

    /// Logs an oracle call. Can return a termination if the oracle call limit is reached.
    fn log_oracle_call(&mut self, result: SolverResult) -> Result<(), Termination> {
        self.stats.n_oracle_calls += 1;
        // Dispatch to logger
        if let Some(logger) = &mut self.logger {
            logger.log_oracle_call(result)?;
        }
        // Update limit and check termination
        if let Some(oracle_calls) = &mut self.lims.oracle_calls {
            *oracle_calls -= 1;
            if *oracle_calls == 0 {
                return Err(Termination::OracleCallsLimit);
            }
        }
        Ok(())
    }

    /// Logs a solution. Can return a termination if the solution limit is reached.
    fn log_solution(&mut self) -> Result<(), Termination> {
        self.stats.n_solutions += 1;
        // Dispatch to logger
        if let Some(logger) = &mut self.logger {
            logger.log_solution()?;
        }
        // Update limit and check termination
        if let Some(solutions) = &mut self.lims.sols {
            *solutions -= 1;
            if *solutions == 0 {
                return Err(Termination::SolsLimit);
            }
        }
        Ok(())
    }

    /// Logs a Pareto point. Can return a termination if the Pareto point limit is reached.
    fn log_pareto_point(&mut self, pareto_point: &NonDomPoint) -> Result<(), Termination> {
        self.stats.n_pareto_points += 1;
        // Dispatch to logger
        if let Some(logger) = &mut self.logger {
            logger.log_pareto_point(pareto_point)?;
        }
        // Update limit and check termination
        if let Some(pps) = &mut self.lims.pps {
            *pps -= 1;
            if *pps == 0 {
                return Err(Termination::PPLimit);
            }
        }
        Ok(())
    }

    /// Logs a heuristic objective improvement. Can return a logger error.
    fn log_heuristic_obj_improvement(
        &mut self,
        obj_idx: usize,
        apparent_cost: usize,
        improved_cost: usize,
    ) -> Result<(), Termination> {
        // Dispatch to logger
        if let Some(logger) = &mut self.logger {
            logger.log_heuristic_obj_improvement(obj_idx, apparent_cost, improved_cost)?;
        }
        Ok(())
    }
}

#[cfg(feature = "oracle-term")]
impl<VM, O: Terminate<'static>, BCG> SolverKernel<VM, O, BCG> {
    fn attach_terminator(&mut self, term_cb: fn() -> ControlSignal) {
        self.term_cb = Some(term_cb);
        self.oracle.attach_terminator(term_cb);
    }
}

#[cfg(not(feature = "oracle-term"))]
impl<VM, O, BCG> SolverKernel<VM, O, BCG> {
    fn attach_terminator(&mut self, term_cb: fn() -> ControlSignal) {
        self.term_cb = Some(term_cb);
    }
}

impl<VM, O, BCG> SolverKernel<VM, O, BCG>
where
    VM: ManageVars,
    O: SolveIncremental + FlipLit,
{
    /// Gets the current objective costs without offset or multiplier. The phase
    /// parameter is needed to determine if the solution should be heuristically
    /// improved.
    fn get_solution_and_internal_costs(
        &mut self,
        tightening: bool,
    ) -> Result<(Vec<usize>, Assignment), Termination> {
        let mut costs = Vec::new();
        costs.resize(self.stats.n_objs, 0);
        let mut sol = self.oracle.solution(self.var_manager.max_var().unwrap())?;
        let costs = (0..self.objs.len())
            .map(|idx| self.get_cost_with_heuristic_improvements(idx, &mut sol, tightening))
            .collect::<Result<Vec<usize>, _>>()?;
        debug_assert_eq!(costs.len(), self.stats.n_objs);
        Ok((costs, sol))
    }

    /// Performs heuristic solution improvement and computes the improved
    /// (internal) cost for one objective
    fn get_cost_with_heuristic_improvements(
        &mut self,
        obj_idx: usize,
        sol: &mut Assignment,
        tightening: bool,
    ) -> Result<usize, Termination> {
        debug_assert!(obj_idx < self.stats.n_objs);
        let mut reduction = 0;
        // TODO: iterate over objective literals by weight
        let mut cost = 0;
        for (l, w) in self.objs[obj_idx].iter() {
            let val = sol.lit_value(l);
            if val == TernaryVal::True {
                if tightening && !self.obj_lit_data.contains_key(&!l) {
                    // If tightening and the negated literal does not appear in
                    // any objective
                    if self.oracle.flip_lit(!l)? {
                        sol.assign_lit(!l);
                        reduction += w;
                        continue;
                    }
                }
                cost += w;
            }
        }
        if tightening {
            self.log_heuristic_obj_improvement(obj_idx, cost + reduction, cost)?;
        }
        Ok(cost)
    }
}

impl<VM, O, BCG> SolverKernel<VM, O, BCG>
where
    VM: ManageVars,
    O: PhaseLit,
{
    /// If solution-guided search is turned on, phases the entire solution in
    /// the oracle
    fn phase_solution(&mut self, solution: Assignment) -> Result<(), Termination> {
        if !self.opts.solution_guided_search {
            return Ok(());
        }
        for lit in solution.into_iter() {
            self.oracle.phase_lit(lit)?;
        }
        Ok(())
    }

    /// If solution-guided search is turned on, unphases every variable in the
    /// solver
    fn unphase_solution(&mut self) -> Result<(), Termination> {
        if !self.opts.solution_guided_search {
            return Ok(());
        }
        for idx in 0..self.var_manager.max_var().unwrap().idx() + 1 {
            self.oracle.unphase_var(var![idx])?;
        }
        Ok(())
    }
}

impl<VM, O, BCG> SolverKernel<VM, O, BCG>
where
    VM: ManageVars,
    O: SolveIncremental + PhaseLit,
    BCG: FnMut(Assignment) -> Clause,
{
    /// Yields Pareto-optimal solutions. The given assumptions must only allow
    /// for solutions at the non-dominated point with given cost. If the options
    /// ask for enumeration, will enumerate all solutions at this point.
    fn yield_solutions(
        &mut self,
        costs: Vec<usize>,
        assumps: Vec<Lit>,
        mut solution: Assignment,
    ) -> Result<(), Termination> {
        debug_assert_eq!(costs.len(), self.stats.n_objs);
        self.unphase_solution()?;

        // Create Pareto point
        let mut pareto_point = NonDomPoint::new(self.externalize_internal_costs(costs));

        // Truncate internal solution to only include original variables
        solution = solution.truncate(self.max_orig_var);

        loop {
            // TODO: add debug assert checking solution cost
            pareto_point.add_sol(solution.clone());
            match self.log_solution() {
                Ok(_) => (),
                Err(term) => {
                    let pp_term = self.log_pareto_point(&pareto_point);
                    self.pareto_front.add_pp(pareto_point);
                    pp_term?;
                    return Err(term);
                }
            }
            if match self.opts.enumeration {
                EnumOptions::NoEnum => true,
                EnumOptions::Solutions(Some(limit)) => pareto_point.n_sols() >= limit,
                EnumOptions::PMCSs(Some(limit)) => pareto_point.n_sols() >= limit,
                _unlimited => false,
            } {
                let pp_term = self.log_pareto_point(&pareto_point);
                self.pareto_front.add_pp(pareto_point);
                return pp_term;
            }
            self.check_terminator()?;

            // Block last solution
            match self.opts.enumeration {
                EnumOptions::Solutions(_) => {
                    self.oracle.add_clause((self.block_clause_gen)(solution))?
                }
                EnumOptions::PMCSs(_) => self.oracle.add_clause(self.block_pareto_mcs(solution))?,
                EnumOptions::NoEnum => panic!("Should never reach this"),
            }

            // Find next solution
            let res = self.oracle.solve_assumps(assumps.clone())?;
            if res == SolverResult::Interrupted {
                return Err(Termination::Callback);
            }
            self.log_oracle_call(res)?;
            if res == SolverResult::Unsat {
                let pp_term = self.log_pareto_point(&pareto_point);
                // All solutions enumerated
                self.pareto_front.add_pp(pareto_point);
                return pp_term;
            }
            self.check_terminator()?;
            solution = self.oracle.solution(self.max_orig_var)?;
        }
    }
}

impl<VM, O, BCG> SolverKernel<VM, O, BCG>
where
    VM: ManageVars,
    O: SolveIncremental,
{
    /// Gets a clause blocking solutions (weakly) dominated by the given cost point,
    /// given objective encodings.
    fn dominated_block_clause<PBE, CE>(
        &mut self,
        costs: &Vec<usize>,
        obj_encs: &mut Vec<ObjEncoding<PBE, CE>>,
    ) -> Clause
    where
        PBE: pb::BoundUpperIncremental,
        CE: card::BoundUpperIncremental,
    {
        debug_assert_eq!(costs.len(), obj_encs.len());
        costs
            .iter()
            .enumerate()
            .filter_map(|(idx, &cst)| {
                // Don't block
                if cst == 0 {
                    return None;
                }
                let enc = &mut obj_encs[idx];
                if let ObjEncoding::Constant = enc {
                    return None;
                }
                // Encode and add to solver
                self.oracle
                    .add_cnf(enc.encode_ub_change(cst - 1..cst, &mut self.var_manager))
                    .unwrap();
                let assumps = enc.enforce_ub(cst - 1).unwrap();
                if assumps.len() == 1 {
                    Some(assumps[0])
                } else {
                    let mut and_impl = Cnf::new();
                    let and_lit = self.var_manager.new_var().pos_lit();
                    and_impl.add_lit_impl_cube(and_lit, assumps);
                    self.oracle.add_cnf(and_impl).unwrap();
                    Some(and_lit)
                }
            })
            .collect()
    }
}

impl<VM, O, BCG> SolverKernel<VM, O, BCG>
where
    O: SolveIncremental,
{
    /// Wrapper around the oracle with call logging and interrupt detection.
    /// Assumes that the oracle is unlimited.
    fn solve(&mut self) -> Result<SolverResult, Termination> {
        let res = self.oracle.solve()?;
        if res == SolverResult::Interrupted {
            return Err(Termination::Callback);
        }
        self.log_oracle_call(res)?;
        Ok(res)
    }

    /// Wrapper around the oracle with call logging and interrupt detection.
    /// Assumes that the oracle is unlimited.
    fn solve_assumps(&mut self, assumps: Vec<Lit>) -> Result<SolverResult, Termination> {
        let res = self.oracle.solve_assumps(assumps)?;
        if res == SolverResult::Interrupted {
            return Err(Termination::Callback);
        }
        self.log_oracle_call(res)?;
        Ok(res)
    }
}

fn absorb_objective<VM: ManageVars>(
    obj: rustsat::instances::Objective,
    cnf: &mut Cnf,
    blits: &mut RsHashMap<Clause, Lit>,
    vm: &mut VM,
) -> Objective {
    if obj.is_empty() {
        return Objective::Constant {
            offset: obj.offset(),
        };
    }
    if obj.weighted() {
        let (soft_cls, offset) = obj.as_soft_cls();
        let lits_iter = soft_cls
            .into_iter()
            .map(|(cl, w)| (absorb_soft_clause(cl, cnf, blits, vm), w));
        Objective::Weighted {
            offset,
            lits: lits_iter.collect(),
        }
    } else {
        let (soft_cls, unit_weight, offset) = obj.as_unweighted_soft_cls();
        let lits_iter = soft_cls
            .into_iter()
            .map(|cl| absorb_soft_clause(cl, cnf, blits, vm));
        Objective::Unweighted {
            offset,
            unit_weight,
            lits: lits_iter.collect(),
        }
    }
}

fn absorb_soft_clause<VM: ManageVars>(
    mut cl: Clause,
    cnf: &mut Cnf,
    blits: &mut RsHashMap<Clause, Lit>,
    vm: &mut VM,
) -> Lit {
    if cl.len() == 1 {
        // No blit needed
        return !cl[0];
    }
    if blits.contains_key(&cl) {
        // Reuse clause with blit that was already added
        return blits[&cl];
    }
    // Get new blit
    let blit = vm.new_var().pos_lit();
    // Save blit in case same soft clause reappears
    // TODO: find way to not have to clone the clause here
    blits.insert(cl.clone(), blit);
    // Relax clause and add it to the CNF
    cl.add(blit);
    cnf.add_clause(cl);
    blit
}

/// Data regarding an objective
enum Objective {
    Weighted {
        offset: isize,
        lits: RsHashMap<Lit, usize>,
    },
    Unweighted {
        offset: isize,
        unit_weight: usize,
        lits: Vec<Lit>,
    },
    Constant {
        offset: isize,
    },
}

impl Objective {
    /// Gets the offset of the encoding
    fn offset(&self) -> isize {
        match self {
            Objective::Weighted { offset, .. } => *offset,
            Objective::Unweighted { offset, .. } => *offset,
            Objective::Constant { offset } => *offset,
        }
    }

    /// Unified iterator over encodings
    fn iter(&self) -> ObjIter<'_> {
        match self {
            Objective::Weighted { lits, .. } => ObjIter::Weighted(lits.iter()),
            Objective::Unweighted { lits, .. } => ObjIter::Unweighted(lits.iter()),
            Objective::Constant { .. } => ObjIter::Constant,
        }
    }
}

enum ObjIter<'a> {
    Weighted(std::collections::hash_map::Iter<'a, Lit, usize>),
    Unweighted(std::slice::Iter<'a, Lit>),
    Constant,
}

impl Iterator for ObjIter<'_> {
    type Item = (Lit, usize);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ObjIter::Weighted(iter) => iter.next().map(|(&l, &w)| (l, w)),
            ObjIter::Unweighted(iter) => iter.next().map(|&l| (l, 1)),
            ObjIter::Constant => None,
        }
    }
}

/// An objective encoding for either a weighted or an unweighted objective
enum ObjEncoding<PBE, CE> {
    Weighted(PBE),
    Unweighted(CE),
    Constant,
}

impl<PBE, CE> ObjEncoding<PBE, CE>
where
    PBE: pb::BoundUpperIncremental,
    CE: card::BoundUpperIncremental,
{
    /// Initializes a new objective encoding for a weighted objective
    fn new_weighted<VM: ManageVars, LI: WLitIter>(
        lits: LI,
        reserve: bool,
        var_manager: &mut VM,
    ) -> Self {
        let mut encoding = PBE::from_iter(lits);
        if reserve {
            encoding.reserve(var_manager);
        }
        ObjEncoding::Weighted(encoding)
    }

    /// Initializes a new objective encoding for a weighted objective
    fn new_unweighted<VM: ManageVars, LI: LitIter>(
        lits: LI,
        reserve: bool,
        var_manager: &mut VM,
    ) -> Self {
        let mut encoding = CE::from_iter(lits);
        if reserve {
            encoding.reserve(var_manager);
        }
        ObjEncoding::Unweighted(encoding)
    }

    /// Gets the next higher objective value
    fn next_higher(&self, val: usize) -> usize {
        match self {
            ObjEncoding::Weighted(enc) => enc.next_higher(val),
            ObjEncoding::Unweighted(_) => val + 1,
            ObjEncoding::Constant => val,
        }
    }

    /// Encodes the given range
    fn encode_ub_change(&mut self, range: Range<usize>, var_manager: &mut dyn ManageVars) -> Cnf {
        match self {
            ObjEncoding::Weighted(enc) => enc.encode_ub_change(range, var_manager),
            ObjEncoding::Unweighted(enc) => enc.encode_ub_change(range, var_manager),
            ObjEncoding::Constant => Cnf::new(),
        }
    }

    /// Enforces the given upper bound
    fn enforce_ub(&mut self, ub: usize) -> Result<Vec<Lit>, rustsat::encodings::Error> {
        match self {
            ObjEncoding::Weighted(enc) => enc.enforce_ub(ub),
            ObjEncoding::Unweighted(enc) => enc.enforce_ub(ub),
            ObjEncoding::Constant => Ok(vec![]),
        }
    }
}

/// Data regarding an objective literal
struct ObjLitData {
    /// Objectives that the literal appears in
    objs: Vec<usize>,
}

/// The default blocking clause generator
pub fn default_blocking_clause(sol: Assignment) -> Clause {
    Clause::from_iter(sol.into_iter().map(Lit::not))
}