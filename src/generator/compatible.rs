//! Compatible interface property queries from cmGeneratorTarget.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use crate::messages::MessageType;
use crate::state::{Target, TargetKind};
use crate::util::{expand_list, is_on};

use super::{ctx::GenCtx, link};

const PIC: &str = "POSITION_INDEPENDENT_CODE";

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Bool,
    String,
    Min,
    Max,
}

impl Kind {
    const ALL: [Self; 4] = [Self::Bool, Self::String, Self::Min, Self::Max];

    fn declaration(self) -> &'static str {
        match self {
            Self::Bool => "COMPATIBLE_INTERFACE_BOOL",
            Self::String => "COMPATIBLE_INTERFACE_STRING",
            Self::Min => "COMPATIBLE_INTERFACE_NUMBER_MIN",
            Self::Max => "COMPATIBLE_INTERFACE_NUMBER_MAX",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Bool => "Boolean compatibility",
            Self::String => "String compatibility",
            Self::Min => "Numeric minimum compatibility",
            Self::Max => "Numeric maximum compatibility",
        }
    }

    fn declarations(self, target: &Target) -> Vec<String> {
        expand_list(
            target.props.get(self.declaration()).unwrap_or_default(),
            false,
        )
    }
}

pub(super) fn property(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    prop: &str,
    evaluating_links: bool,
) -> Option<String> {
    if evaluating_links
        || matches!(
            target.kind,
            TargetKind::InterfaceLibrary | TargetKind::ObjectLibrary
        )
    {
        return None;
    }
    let raw = target.props.get(prop);
    if prop == PIC && raw.is_some() {
        return None;
    }
    let deps = link::compile_implementation_closure(ctx, target, config);
    for kind in Kind::ALL {
        if matches!(kind, Kind::Bool | Kind::String) && raw.is_some() {
            continue;
        }
        if (kind == Kind::Bool && prop == PIC)
            || (kind == Kind::String && prop == "AUTOUIC_OPTIONS")
            || deps
                .iter()
                .any(|dep| kind.declarations(dep).iter().any(|item| item == prop))
        {
            let value = compatible_value(ctx, target, config, prop, kind, &deps);
            return Some(if kind == Kind::Bool {
                if value.as_deref().is_some_and(is_on) {
                    "1"
                } else {
                    "0"
                }
                .to_owned()
            } else {
                value.unwrap_or_default()
            });
        }
    }
    None
}

pub(super) fn pic(ctx: &GenCtx, target: &Target, config: &str) -> Option<bool> {
    let deps = link::compile_implementation_closure(ctx, target, config);
    compatible_value(ctx, target, config, PIC, Kind::Bool, &deps).map(|value| is_on(&value))
}

pub(super) fn implied_key(ctx: &GenCtx, target: &Target, prop: &str) -> String {
    format!("compatible:link-implied:{}:{prop}", ctx.target_key(target))
}

pub(super) fn check_declarations<'a>(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    link_items: impl Iterator<Item = &'a Target>,
) {
    let mut emitted: [BTreeSet<String>; 4] = std::array::from_fn(|_| BTreeSet::new());
    let mut deps = None;
    for dep in link_items.filter(|dep| dep.kind != TargetKind::ObjectLibrary) {
        for (kind, emitted) in Kind::ALL.into_iter().zip(&mut emitted) {
            for prop in kind.declarations(dep) {
                let declaration = kind.declaration();
                let help_name = prop.replace(['<', '>'], "");
                let root = Path::new(&ctx.interp.cmake_root);
                if root
                    .join("Help/prop_tgt")
                    .join(format!("{help_name}.rst"))
                    .is_file()
                {
                    ctx.error(ctx.dir_backtrace(target.makefile), format!(
                        "Target \"{}\" has property \"{prop}\" listed in its {declaration} property.  This is not allowed.  Only user-defined properties may appear listed in the {declaration} property.",
                        dep.name
                    ));
                    return;
                }
                if emitted.insert(prop.clone()) {
                    let deps = deps.get_or_insert_with(|| {
                        link::compile_implementation_closure(ctx, target, config)
                    });
                    compatible_value(ctx, target, config, &prop, kind, deps);
                    if ctx.has_error() {
                        return;
                    }
                }
            }
        }
    }
    for (index, left) in emitted.iter().enumerate() {
        for right in &emitted[index + 1..] {
            if let Some(prop) = left.intersection(right).next() {
                let mut declarations: Vec<_> = Kind::ALL
                    .iter()
                    .zip(&emitted)
                    .filter_map(|(kind, props)| props.contains(prop).then_some(kind.declaration()))
                    .collect();
                declarations.sort_unstable();
                let last = declarations.pop().unwrap();
                ctx.error(ctx.dir_backtrace(target.makefile), format!(
                    "Property \"{prop}\" appears in both the {} and the {last} property in the dependencies of target \"{}\".  This is not allowed. A property may only require compatibility in a boolean interpretation, a numeric minimum, a numeric maximum or a string interpretation, but not a mixture.",
                    declarations.join(", "), target.name
                ));
                return;
            }
        }
    }
}

fn compatible_value(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    prop: &str,
    kind: Kind,
    deps: &[&Target],
) -> Option<String> {
    let key = format!(
        "compatible:{}:{config}:{}:{prop}",
        kind.declaration(),
        ctx.target_key(target)
    );
    if let Some(cached) = ctx.memo.borrow().get(&key) {
        return cached.strip_prefix('=').map(str::to_owned);
    }
    let raw = target.props.get(prop);
    if deps.is_empty() {
        return raw.map(str::to_owned);
    }
    let implied = raw.is_none()
        && ctx
            .memo
            .borrow()
            .contains_key(&implied_key(ctx, target, prop));
    let explicit = raw.is_some();
    let normalize = |text: &str| {
        if kind == Kind::Bool {
            if is_on(text) { "TRUE" } else { "FALSE" }.to_owned()
        } else {
            text.to_owned()
        }
    };
    let mut value = raw.map(normalize);
    let interface_prop = format!("INTERFACE_{prop}");
    let mut report = format!(" * Target \"{}\" ", target.name);
    if let Some(value) = &value {
        let _ = writeln!(report, "has property content \"{value}\"");
    } else if implied {
        report.push_str("property is implied by use.\n");
    } else {
        report.push_str("property not set.\n");
    }
    // The value can itself refer to a compatible property via TARGET_PROPERTY.
    ctx.memo.borrow_mut().insert(key.clone(), String::new());
    for dep in deps {
        if implied {
            value = Some(normalize(""));
        }
        let Some(raw) = dep.props.get(&interface_prop) else {
            continue;
        };
        let interface = if prop == PIC {
            ctx.genex_with_dag(raw, target, target, &interface_prop, None)
        } else {
            raw.to_owned()
        };
        let interface = normalize(&interface);
        let _ = write!(
            report,
            " * Target \"{}\" property value \"{interface}\" ",
            dep.name
        );
        let Some(current) = value.as_ref() else {
            report.push_str("(Interface set)\n");
            value = Some(interface);
            continue;
        };

        let consistent = match kind {
            Kind::Bool | Kind::String => (current == &interface).then_some(current),
            Kind::Min | Kind::Max => {
                number(current)
                    .zip(number(&interface))
                    .map(|(left, right)| {
                        if (kind == Kind::Min && right < left)
                            || (kind == Kind::Max && right > left)
                        {
                            &interface
                        } else {
                            current
                        }
                    })
            }
        };
        let changed = consistent != Some(current);
        report.push_str(match (kind, changed) {
            (Kind::Bool, _) | (Kind::String, false) => "(Agree)\n",
            (Kind::String, true) => "(Disagree)\n",
            (_, false) => "(Ignored)\n",
            (_, true) => "(Dominant)\n",
        });
        if let Some(consistent) = consistent {
            value = Some(consistent.clone());
        } else {
            let message = if explicit {
                format!(
                    "Property {prop} on target \"{}\" does\nnot match the {interface_prop} property requirement\nof dependency \"{}\".\n",
                    target.name, dep.name
                )
            } else if implied {
                let default = if kind == Kind::Bool { "FALSE" } else { "empty" };
                format!(
                    "Property {prop} on target \"{}\" is\nimplied to be {default} because it was used to determine the link libraries\nalready. The {interface_prop} property on\ndependency \"{}\" is in conflict.\n",
                    target.name, dep.name
                )
            } else {
                format!(
                    "The {interface_prop} property of \"{}\" does\nnot agree with the value of {prop} already determined\nfor \"{}\".\n",
                    dep.name, target.name
                )
            };
            ctx.system_error(message);
            break;
        }
    }
    if expand_list(
        &ctx.var_safe(target.makefile, "CMAKE_DEBUG_TARGET_PROPERTIES"),
        false,
    )
    .iter()
    .any(|item| item == prop)
        && ctx
            .memo
            .borrow_mut()
            .insert(
                format!("compatible:reported:{}:{prop}", ctx.target_key(target)),
                String::new(),
            )
            .is_none()
    {
        let result = value.as_deref().unwrap_or(if kind == Kind::Bool {
            "FALSE"
        } else {
            "(unset)"
        });
        ctx.message(
            MessageType::Log,
            Vec::new(),
            format!(
                "{} of property \"{prop}\" for target \"{}\" (result: \"{result}\"):\n{report}",
                kind.description(),
                target.name
            ),
        );
    }
    ctx.memo.borrow_mut().insert(
        key,
        value
            .as_ref()
            .map_or_else(String::new, |value| format!("={value}")),
    );
    value
}

fn number(text: &str) -> Option<i64> {
    let text = text.trim_start_matches(|ch: char| ch.is_ascii_whitespace() || ch == '\u{b}');
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    let (digits, radix) = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        (hex, 16)
    } else if digits.starts_with('0') {
        (digits, 8)
    } else {
        (digits, 10)
    };
    if digits.starts_with(['+', '-']) {
        return None;
    }
    let value = i128::from_str_radix(digits, radix).ok()?;
    i64::try_from(if text.starts_with('-') { -value } else { value }).ok()
}
