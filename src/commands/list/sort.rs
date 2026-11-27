//! Sorting shared by `list(SORT)` and `$<LIST:SORT>`, as in `cmList::sort`.

use std::cmp::Ordering;
use std::ffi::c_char;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompareMethod {
    String,
    FileBasename,
    Natural,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaseSensitivity {
    Sensitive,
    Insensitive,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrderMode {
    Ascending,
    Descending,
}

pub(crate) struct SortConfig {
    pub(crate) compare: CompareMethod,
    pub(crate) case: CaseSensitivity,
    pub(crate) order: OrderMode,
}

impl SortConfig {
    pub(crate) fn sort(&self, values: &mut [String]) {
        let ordering = |ord: Ordering| match self.order {
            OrderMode::Ascending => ord,
            OrderMode::Descending => ord.reverse(),
        };
        if self.compare == CompareMethod::String && self.case == CaseSensitivity::Sensitive {
            // With no filters, two equivalent keys are identical values.
            values.sort_unstable_by(|a, b| {
                ordering(crate::bytes::encode(a).cmp(&crate::bytes::encode(b)))
            });
            return;
        }

        let mut entries: Vec<_> = values
            .iter_mut()
            .map(|value| {
                let encoded = crate::bytes::encode(value);
                let bytes = if self.compare == CompareMethod::FileBasename {
                    encoded
                        .rsplit(|&b| b == b'/' || (cfg!(windows) && b == b'\\'))
                        .next()
                        .unwrap_or_default()
                } else {
                    &encoded
                };
                let mut key = bytes.to_vec();
                if self.case == CaseSensitivity::Insensitive {
                    key.make_ascii_lowercase();
                }
                (std::mem::take(value), key)
            })
            .collect();
        libstdcxx_sort(&mut entries, |(_, a), (_, b)| {
            let ord = if self.compare == CompareMethod::Natural {
                strverscmp(a, b)
            } else {
                a.cmp(b)
            };
            ordering(ord).is_lt()
        });
        for (slot, (value, _)) in values.iter_mut().zip(entries) {
            *slot = value;
        }
    }
}

/// `cmSystemTools::strverscmp`, which compares NUL-terminated byte strings.
fn strverscmp(left: &[u8], right: &[u8]) -> Ordering {
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while at(left, i) == at(right, i) && at(left, i) != 0 {
        i += 1;
    }
    let a = at(left, i);
    let b = at(right, i);
    if a != b {
        let begin = i - left[..i]
            .iter()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if begin != i || (a.is_ascii_digit() && b.is_ascii_digit()) {
            let zeros = |s: &[u8]| {
                s[begin..]
                    .windows(2)
                    .take_while(|pair| pair[0] == b'0' && pair[1].is_ascii_digit())
                    .count()
            };
            let left_zeros = zeros(left);
            let right_zeros = zeros(right);
            if left_zeros != right_zeros {
                return right_zeros.cmp(&left_zeros);
            }
            if left_zeros == 0 {
                let digits = |s: &[u8]| s[i..].iter().take_while(|c| c.is_ascii_digit()).count();
                let digit_order = digits(left).cmp(&digits(right));
                if digit_order != Ordering::Equal {
                    return digit_order;
                }
            }
        }
    }
    (a as c_char).cmp(&(b as c_char))
}

const INSERTION_THRESHOLD: usize = 16;

/// The pinned CMake uses libstdc++'s `std::sort`. Equivalent keys can have
/// different original values, so its pivot choices and heap tie breaks
/// determine the observable order.
fn libstdcxx_sort<T>(values: &mut [T], less: impl Fn(&T, &T) -> bool) {
    if values.is_empty() {
        return;
    }
    introsort(values, 2 * values.len().ilog2(), &less);
    // libstdc++ insertion-sorts the first 16 elements, then uses an
    // unguarded insertion sort. Both only move strictly lesser values.
    for i in 1..values.len() {
        let mut pos = i;
        while pos > 0 && less(&values[pos], &values[pos - 1]) {
            values.swap(pos, pos - 1);
            pos -= 1;
        }
    }
}

fn introsort<T>(mut values: &mut [T], mut depth: u32, less: &impl Fn(&T, &T) -> bool) {
    while values.len() > INSERTION_THRESHOLD {
        if depth == 0 {
            heap_sort(values, less);
            return;
        }
        depth -= 1;
        let cut = partition(values, less);
        let (left, right) = values.split_at_mut(cut);
        introsort(right, depth, less);
        values = left;
    }
}

fn partition<T>(values: &mut [T], less: &impl Fn(&T, &T) -> bool) -> usize {
    let a = 1;
    let b = values.len() / 2;
    let c = values.len() - 1;
    let lesser = |i: usize, j: usize| less(&values[i], &values[j]);
    let median = if lesser(a, b) {
        if lesser(b, c) {
            b
        } else if lesser(a, c) {
            c
        } else {
            a
        }
    } else if lesser(a, c) {
        a
    } else if lesser(b, c) {
        c
    } else {
        b
    };
    values.swap(0, median);

    let mut left = 1;
    let mut right = values.len();
    loop {
        while less(&values[left], &values[0]) {
            left += 1;
        }
        right -= 1;
        while less(&values[0], &values[right]) {
            right -= 1;
        }
        if left >= right {
            return left;
        }
        values.swap(left, right);
        left += 1;
    }
}

fn heap_sort<T>(values: &mut [T], less: &impl Fn(&T, &T) -> bool) {
    for parent in (0..values.len() / 2).rev() {
        adjust_heap(values, parent, less);
    }
    for end in (1..values.len()).rev() {
        values.swap(0, end);
        adjust_heap(&mut values[..end], 0, less);
    }
}

fn adjust_heap<T>(values: &mut [T], top: usize, less: &impl Fn(&T, &T) -> bool) {
    // Move the old root down to a leaf before pushing it back up. Swapping
    // keeps the value at the hole and preserves libstdc++'s moves when
    // children compare equal and the right child is promoted.
    let mut hole = top;
    while hole < values.len() / 2 {
        let left = 2 * hole + 1;
        let right = left + 1;
        let child = if right < values.len() && !less(&values[right], &values[left]) {
            right
        } else {
            left
        };
        values.swap(hole, child);
        hole = child;
    }
    while hole > top {
        let parent = (hole - 1) / 2;
        if !less(&values[parent], &values[hole]) {
            break;
        }
        values.swap(parent, hole);
        hole = parent;
    }
}
