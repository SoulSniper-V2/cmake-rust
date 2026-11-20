use std::cell::RefCell;
use std::collections::HashSet;

use crate::genex::{Backtrace, Evaluator, GenexError, Scope};

/// One cmGeneratorExpressionDAGChecker frame. The generator can supply a root
/// to identify which property is being evaluated.
#[derive(Debug)]
pub struct DagChecker<'a> {
    pub target: &'a str,
    pub property: &'a str,
    pub expression: Option<&'a str>,
    pub parent: Option<&'a DagChecker<'a>>,
    pub backtrace: Option<Backtrace>,
    pub transitive: bool,
    pub transitive_properties_only: bool,
    pub transitive_properties_only_cmp0131: bool,
    pub computing_link_libraries: bool,
    /// cmGeneratorExpressionDAGChecker::Seen, meaningful on the top checker.
    pub seen: RefCell<HashSet<(String, String)>>,
}

#[derive(PartialEq, Eq)]
pub(super) enum Check {
    Dag,
    SelfReference,
    Cycle,
    AlreadySeen,
}

impl<'a> DagChecker<'a> {
    pub fn new(target: &'a str, property: &'a str) -> Self {
        Self {
            target,
            property,
            expression: None,
            parent: None,
            backtrace: None,
            transitive: false,
            transitive_properties_only: false,
            transitive_properties_only_cmp0131: false,
            computing_link_libraries: false,
            seen: RefCell::default(),
        }
    }

    pub(super) fn root(&self) -> &Self {
        let mut root = self;
        while let Some(parent) = root.parent {
            root = parent;
        }
        root
    }

    pub(super) fn link_libraries(&self, markers: bool) -> bool {
        let prop = self.root().property;
        matches!(
            prop,
            "LINK_LIBRARIES"
                | "INTERFACE_LINK_LIBRARIES"
                | "INTERFACE_LINK_LIBRARIES_DIRECT"
                | "LINK_INTERFACE_LIBRARIES"
                | "IMPORTED_LINK_INTERFACE_LIBRARIES"
        ) || prop.starts_with("LINK_INTERFACE_LIBRARIES_")
            || prop.starts_with("IMPORTED_LINK_INTERFACE_LIBRARIES_")
            || (!markers && prop == "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE")
    }

    pub(super) fn compile_expression(&self) -> bool {
        matches!(
            self.root().property,
            "INCLUDE_DIRECTORIES" | "COMPILE_DEFINITIONS" | "COMPILE_OPTIONS"
        )
    }

    pub(super) fn link_options(&self) -> bool {
        matches!(self.root().property, "LINK_OPTIONS" | "LINKER_TYPE")
    }

    pub(super) fn link_language(&self) -> bool {
        self.link_options()
            || self.link_libraries(false)
            || matches!(
                self.root().property,
                "LINK_DIRECTORIES" | "LINK_DEPENDS" | "LINK_LIBRARY_OVERRIDE"
            )
            || self.linker_launcher()
    }

    pub(super) fn linker_launcher(&self) -> bool {
        let prop = self.root().property;
        prop.len() > "_LINKER_LAUNCHER".len() && prop.ends_with("_LINKER_LAUNCHER")
    }

    pub(super) fn sources(&self) -> bool {
        matches!(self.property, "SOURCES" | "INTERFACE_SOURCES")
    }

    pub(super) fn genex(&self) -> bool {
        self.property.starts_with("GENEX_EVAL:") || self.property.starts_with("TARGET_GENEX_EVAL:")
    }
}

impl Evaluator<'_, '_> {
    pub(super) fn check(&self, frame: &DagChecker<'_>) -> Check {
        let mut ancestor = frame.parent;
        let mut direct = true;
        while let Some(parent) = ancestor {
            if frame.target == parent.target && frame.property == parent.property {
                return if direct {
                    Check::SelfReference
                } else {
                    Check::Cycle
                };
            }
            ancestor = parent.parent;
            direct = false;
        }
        let root = frame.root();
        if root.transitive
            && !root
                .seen
                .borrow_mut()
                .insert((frame.target.to_owned(), frame.property.to_owned()))
        {
            return Check::AlreadySeen;
        }
        Check::Dag
    }

    pub(super) fn cycle_error(
        &self,
        frame: &DagChecker<'_>,
        expr: &str,
        scope: Scope<'_>,
    ) -> GenexError {
        if let Some(parent) = frame.parent.filter(|parent| parent.parent.is_none()) {
            let mut error = self.error(
                expr,
                format!(
                    "Self reference on target \"{}\".\n",
                    scope.head.unwrap_or(parent.target)
                ),
            );
            error.backtrace = parent.backtrace;
            return error;
        }
        let mut error = self.error(expr, "Dependency loop found.");
        let mut next = frame.parent;
        while let Some(parent) = next {
            let step = error.loop_steps.len() + 1;
            error.loop_steps.push((
                format!(
                    "Loop step {step}\n  {}\n",
                    parent.expression.unwrap_or(expr)
                ),
                parent.backtrace,
            ));
            next = parent.parent;
        }
        error
    }
}
