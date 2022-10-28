//! # $P$-Minimal Model Enumeration for Multi-Objective Optimization
//!
//! This library implements $P$-minimal model enumeration as an algorithm for
//! solving multi-objective optimization problems expressed as boolean logic.
//! Instead of using the order encoding as in \[1\], any cardinality (for
//! unweighted objectives) or pseudo-boolean encoding from
//! [RustSAT](https://github.com/chrjabs/rustsat) can be used. The actual
//! enumeration algorithm follows \[2\].
//!
//! ## References
//!
//! - \[1\] Takehide Soh and Mutsunori Banbara and Naoyuki Tamura and Daniel Le
//!   Berre: _Solving Multiobjective Discrete Optimization Problems with
//!   Propositional Minimal Model Generation_, CP 2017.
//! - \[2\] Miyuki Koshimura and Hidetomo Nabeshima and Hiroshi Fujita and Ryuzo
//!   Hasegawa: _Minimal Model Generation with Respect to an Atom Set_, FTP
//!   2009.

use rustsat::instances::{ManageVars, MultiOptInstance};

mod options;
pub use options::Options;

pub mod types;
use types::ParetoFront;

mod pminimal;
pub use pminimal::PMinimal;

/// Main interface for using this multi-objective optimization solver
trait Solve<VM>
where
    VM: ManageVars,
{
    /// Initializes a new solver from a multi-objective optimization instance
    fn init(inst: MultiOptInstance<VM>) -> Self
    where
        Self: Sized,
    {
        Self::init_with_options(inst, Options::default())
    }
    /// Initializes a new solver with given options from a multi-objective
    /// optimization instance
    fn init_with_options(inst: MultiOptInstance<VM>, opts: Options) -> Self;
    /// Solves the instance under given limits. Returns why the call was terminated.
    fn solve(
        &mut self,
        max_pps: Option<usize>,
        max_sols: Option<usize>,
        max_candidates: Option<usize>,
        max_oracle_calls: Option<usize>,
    ) -> Termination;
    /// Gets the Pareto front discovered so far
    fn pareto_front(&self) -> ParetoFront;
    /// Gets tracked statistics from the solver
    fn stats(&self) -> Stats;
}

/// Trait for getting statistics from the solver
trait ExtendedSolveStats {
    /// Gets statistics from the internal oracle
    fn oracle_stats(&self) -> OracleStats;
    /// Gets statistics from the objective encodings
    fn encoding_stats(&self) -> Vec<EncodingStats>;
}

/// Termination reasons for [`Solve::solve`]
pub enum Termination {
    /// Terminated because of maximum number of Pareto points reached
    PPBound,
    /// Terminated because of maximum number of solutions reached
    SolsBound,
    /// Terminated because of maximum number of candidates reached
    CandidatesBound,
    /// Terminated because of maximum number of oracle calls reached
    OracleCallsBound,
    /// Terminated because instance fully solved
    Solved,
}

/// Statistics of the solver
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Stats {
    /// The number of calls to [`Solve::solve`]
    pub n_solve_calls: usize,
    /// The number of Pareto-optimal solutions found
    pub n_solutions: usize,
    /// The number of Pareto points found
    pub n_pareto_points: usize,
    /// The number of candidates explored
    pub n_candidates: usize,
    /// The number of calls to the SAT oracle
    pub n_oracle_calls: usize,
    /// The number of objectives in the solver
    pub n_objs: usize,
}

impl Stats {
    /// Creates a new set of statistics
    fn init() -> Self {
        Stats {
            n_solve_calls: 0,
            n_solutions: 0,
            n_pareto_points: 0,
            n_candidates: 0,
            n_oracle_calls: 0,
            n_objs: 0,
        }
    }
}

/// Statistics of the used SAT solver
#[derive(Debug, PartialEq, Clone)]
pub struct OracleStats {
    /// The number of satisfiable queries
    pub n_sat_solves: u32,
    /// The number of unsatisfiable queries
    pub n_unsat_solves: u32,
    /// The number of irredundant clauses in the solver
    pub n_clauses: u32,
    /// The number of variables in the solver
    pub n_vars: usize,
    /// The average length of irredundant clauses
    pub avg_clause_len: f32,
    /// The total CPU time spent in the oracle
    pub cpu_solve_time: f32,
}

/// Statistics of a used cardinality or pseudo-boolean encodings
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct EncodingStats {
    /// The number of clauses in the encoding
    pub n_clauses: usize,
    /// The number of variables in the encoding
    pub n_vars: usize,
    /// The objective offset
    pub offset: isize,
    /// The unit weight, if the objective is unweighted
    pub unit_weight: Option<usize>,
}
