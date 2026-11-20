//! cmCustomCommandGenerator. OWNER: generator-target worker.

use super::ctx::GenCtx;

/// A custom command evaluated for one configuration.
#[derive(Clone, Debug, Default)]
pub struct EvaluatedCommand {
    /// Each command line as argv (genex-evaluated, executable resolved for
    /// targets).
    pub command_lines: Vec<Vec<String>>,
    pub outputs: Vec<String>,
    pub byproducts: Vec<String>,
    pub depends: Vec<String>,
    pub working_directory: String,
    pub comment: Option<String>,
    pub depfile: String,
    pub job_pool: String,
    pub uses_terminal: bool,
    pub escape_old_style: bool,
    pub escape_allow_make_vars: bool,
}

pub fn evaluate(ctx: &GenCtx) -> EvaluatedCommand {
    let _ = ctx;
    todo!("generator::custom::evaluate")
}
