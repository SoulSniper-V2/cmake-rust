//! Build-system generation (Ninja). Port of cmGlobalNinjaGenerator and
//! friends — in progress.

use crate::configure::ConfigureOptions;
use crate::interp::Interp;

pub fn generate(interp: &mut Interp, options: &ConfigureOptions) -> i32 {
    let _ = options;
    let start = std::time::Instant::now();
    if !options.in_try_compile {
        let elapsed = start.elapsed().as_secs_f64();
        crate::commands::core::status_line(interp, &format!("Generating done ({elapsed:.1}s)"));
    }
    0
}
