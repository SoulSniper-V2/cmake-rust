use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::testfile::Test;

pub(super) type Groups = Vec<Vec<(String, u32)>>;
type Slots = BTreeMap<String, BTreeMap<String, u32>>;

pub(super) fn parse_groups(value: &str) -> Option<Groups> {
    let mut groups = Vec::new();
    for group in value.split(';').filter(|group| !group.is_empty()) {
        let mut entries = group.split(',').filter(|part| !part.is_empty()).peekable();
        let count = if entries
            .peek()
            .is_some_and(|entry| entry.bytes().all(|c| c.is_ascii_digit()))
        {
            entries.next()?.parse().ok()?
        } else {
            1
        };
        let mut requirements = Vec::new();
        for entry in entries {
            let (kind, slots) = entry.split_once(':')?;
            if !valid_name(kind, true)
                || slots.is_empty()
                || !slots.bytes().all(|c| c.is_ascii_digit())
            {
                return None;
            }
            requirements.push((kind.into(), slots.parse().ok()?));
        }
        groups.try_reserve(count).ok()?;
        groups.extend(std::iter::repeat_n(requirements, count));
    }
    Some(groups)
}

fn valid_name(name: &str, kind: bool) -> bool {
    !name.is_empty()
        && (!kind || !name.as_bytes()[0].is_ascii_digit())
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

#[derive(Default)]
pub(super) struct Resources {
    path: PathBuf,
    total: Slots,
    free: Slots,
}

#[derive(Default)]
pub(super) struct Allocation {
    groups: Vec<BTreeMap<String, Vec<(String, u32)>>>,
    active: bool,
}

impl Resources {
    pub fn read(path: &Path) -> Result<Self, String> {
        let error = |detail: &str| {
            format!(
                "Could not read/parse resource spec file {}:\n{detail}\n",
                path.display()
            )
        };
        let data = std::fs::read_to_string(path)
            .map_err(|_| error(&format!("File not found: {}", path.display())))?;
        let root: Value = serde_json::from_str(&data).map_err(|err| {
            let detail = if err.line() == 1 && err.column() == 1 && !data.trim().is_empty() {
                let filename = path
                    .file_name()
                    .map(|name| name.to_string_lossy())
                    .unwrap_or_default();
                let line = data.lines().next().unwrap_or_default();
                format!(
                    "{filename}:1: Syntax error: value, object or array expected.\n{line}\n^\n{filename}:1: Extra non-whitespace after JSON value.\n{line}\n ^"
                )
            } else {
                err.to_string()
            };
            error(&detail)
        })?;
        let version = root
            .get("version")
            .and_then(Value::as_object)
            .ok_or_else(|| error("Missing required field \"version\""))?;
        let field = |name: &str| version.get(name).and_then(Value::as_i64);
        if field("major") != Some(1) || field("minor") != Some(0) {
            return Err(error(""));
        }
        let local = root
            .get("local")
            .and_then(Value::as_array)
            .filter(|entries| entries.len() <= 1)
            .ok_or_else(|| error("Invalid socket object"))?;
        let mut total = Slots::new();
        if let Some(socket) = local.first() {
            for (kind, resources) in socket
                .as_object()
                .ok_or_else(|| error("Invalid socket object"))?
            {
                if !valid_name(kind, true) {
                    continue;
                }
                let mut units = BTreeMap::new();
                for unit in resources
                    .as_array()
                    .ok_or_else(|| error("Invalid resource type object"))?
                {
                    let id = unit
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| valid_name(id, false))
                        .ok_or_else(|| error("Invalid resource object"))?;
                    let slots = unit
                        .get("slots")
                        .map_or(Some(1), |slots| {
                            slots.as_u64().and_then(|n| u32::try_from(n).ok())
                        })
                        .ok_or_else(|| error("Invalid resource object"))?;
                    units.entry(id.into()).or_insert(slots);
                }
                total.insert(kind.clone(), units);
            }
        }
        Ok(Self {
            path: path.into(),
            free: total.clone(),
            total,
        })
    }

    pub fn unavailable(&self, name: &str, groups: &Groups) -> Option<String> {
        if self.path.as_os_str().is_empty() {
            return None;
        }
        let needs = requirements(groups);
        let mut errors = String::new();
        for (kind, mut requests) in needs {
            if let Some(units) = self.total.get(kind) {
                if pack(units, &mut requests).is_some() {
                    continue;
                }
                let _ = writeln!(
                    errors,
                    "  Test requested resources of type '{kind}' in the following amounts:"
                );
                for (_, slots) in groups.iter().flatten().filter(|(key, _)| key == kind) {
                    let _ = writeln!(errors, "    {slots} {}", slot_word(*slots));
                }
                errors.push_str("  but only the following units were available:\n");
                for (id, slots) in units {
                    let _ = writeln!(errors, "    '{id}': {slots} {}", slot_word(*slots));
                }
                errors.push('\n');
            } else {
                let _ = writeln!(
                    errors,
                    "  Test requested resources of type '{kind}' which does not exist\n"
                );
            }
        }
        (!errors.is_empty()).then(|| {
            format!(
                "Insufficient resources for test {name}:\n\n{errors}Resource spec file:\n\n  {}",
                self.path.display()
            )
        })
    }

    pub fn allocate(&mut self, groups: &Groups) -> Option<Allocation> {
        if self.path.as_os_str().is_empty() {
            return Some(Allocation::default());
        }
        let mut allocation = Allocation {
            active: true,
            groups: vec![BTreeMap::new(); groups.len()],
        };
        for (kind, mut requests) in requirements(groups) {
            let units = self.free.get(kind)?;
            let ids = pack(units, &mut requests)?;
            for ((group, slots), id) in requests.into_iter().zip(ids) {
                allocation.groups[group]
                    .entry(kind.into())
                    .or_default()
                    .push((id, slots));
            }
        }
        for (kind, id, slots) in allocation.iter() {
            *self.free.get_mut(kind).unwrap().get_mut(id).unwrap() -= slots;
        }
        Some(allocation)
    }

    pub fn release(&mut self, allocation: &Allocation) {
        for (kind, id, slots) in allocation.iter() {
            *self.free.get_mut(kind).unwrap().get_mut(id).unwrap() += slots;
        }
    }
}

fn slot_word(slots: u32) -> &'static str {
    if slots == 1 { "slot" } else { "slots" }
}

impl Allocation {
    fn iter(&self) -> impl Iterator<Item = (&str, &str, u32)> {
        self.groups.iter().flat_map(|group| {
            group.iter().flat_map(|(kind, units)| {
                units
                    .iter()
                    .map(|(id, slots)| (kind.as_str(), id.as_str(), *slots))
            })
        })
    }

    pub fn environment(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        if self.active {
            env.insert(
                "CTEST_RESOURCE_GROUP_COUNT".into(),
                self.groups.len().to_string(),
            );
            for (group, resources) in self.groups.iter().enumerate() {
                let prefix = format!("CTEST_RESOURCE_GROUP_{group}");
                env.insert(
                    prefix.clone(),
                    resources.keys().cloned().collect::<Vec<_>>().join(","),
                );
                for (kind, units) in resources {
                    env.insert(
                        format!("{prefix}_{}", kind.to_ascii_uppercase()),
                        units
                            .iter()
                            .map(|(id, slots)| format!("id:{id},slots:{slots}"))
                            .collect::<Vec<_>>()
                            .join(";"),
                    );
                }
            }
        }
        env
    }
}

fn requirements(groups: &Groups) -> BTreeMap<&str, Vec<(usize, u32)>> {
    let mut requests: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (group, requirements) in groups.iter().enumerate() {
        for (kind, slots) in requirements {
            requests
                .entry(kind.as_str())
                .or_default()
                .push((group, *slots));
        }
    }
    requests
}

fn pack(units: &BTreeMap<String, u32>, requests: &mut [(usize, u32)]) -> Option<Vec<String>> {
    requests.sort_by_key(|(_, slots)| std::cmp::Reverse(*slots));
    let mut units: Vec<_> = units
        .iter()
        .map(|(id, slots)| (id.as_str(), *slots))
        .collect();
    units.sort_by_key(|(_, slots)| std::cmp::Reverse(*slots));
    let mut result = Vec::with_capacity(requests.len());
    assign(&units, requests, &mut result).then(|| result.into_iter().map(str::to_owned).collect())
}

fn assign<'a>(
    units: &[(&'a str, u32)],
    requests: &[(usize, u32)],
    assigned: &mut Vec<&'a str>,
) -> bool {
    let Some(((_, needed), remaining)) = requests.split_first() else {
        return true;
    };
    let mut previous = None;
    for (index, &(id, capacity)) in units.iter().enumerate() {
        if capacity < *needed {
            break;
        }
        if previous == Some(capacity) {
            continue;
        }
        previous = Some(capacity);
        let mut next = units.to_vec();
        next[index].1 -= needed;
        next[index..].sort_by_key(|(_, slots)| std::cmp::Reverse(*slots));
        assigned.push(id);
        if assign(&next, remaining, assigned) {
            return true;
        }
        assigned.pop();
    }
    false
}

pub(super) fn validate_generated(tests: &[Test], specified: bool) -> Result<(), &'static str> {
    let mut generator = None;
    for test in tests {
        let path = test.property("GENERATED_RESOURCE_SPEC_FILE");
        if path.is_empty() {
            continue;
        }
        if generator.is_some() {
            return Err("Only one test may define the GENERATED_RESOURCE_SPEC_FILE property");
        }
        let [fixture] = test.list("FIXTURES_SETUP") else {
            return Err(
                "Test that defines GENERATED_RESOURCE_SPEC_FILE must have exactly one FIXTURES_SETUP",
            );
        };
        if !Path::new(path).is_absolute() {
            return Err("GENERATED_RESOURCE_SPEC_FILE must be an absolute path");
        }
        generator = Some(fixture);
    }
    if let Some(fixture) = generator {
        if tests.iter().any(|test| {
            !test.property("RESOURCE_GROUPS").is_empty()
                && !test.list("FIXTURES_REQUIRED").contains(fixture)
        }) {
            return Err(
                "All tests that have RESOURCE_GROUPS must include the resource spec generator fixture in their FIXTURES_REQUIRED",
            );
        }
        if specified {
            return Err(
                "GENERATED_RESOURCE_SPEC_FILE test property cannot be used in conjunction with ResourceSpecFile option",
            );
        }
    }
    Ok(())
}
