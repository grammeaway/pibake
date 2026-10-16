//! Line diff (longest common subsequence) for `verify` reports.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

/// Above this many line pairs the middle is reported as a whole replacement.
const MAX_TABLE: usize = 16_000_000;

pub fn lines<'a>(a: &'a str, b: &'a str) -> Vec<Op<'a>> {
    let (a, b): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops: Vec<Op> = a[..prefix].iter().map(|l| Op::Equal(l)).collect();
    ops.extend(middle(am, bm));
    ops.extend(a[a.len() - suffix..].iter().map(|l| Op::Equal(l)));
    ops
}

fn middle<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Op<'a>> {
    let (n, m) = (a.len(), b.len());
    if n * m > MAX_TABLE {
        let deletes = a.iter().map(|l| Op::Delete(l));
        return deletes.chain(b.iter().map(|l| Op::Insert(l))).collect();
    }
    // lcs[i][j]: LCS length of a[i..] and b[j..].
    let mut lcs = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[at(i, j)] = if a[i] == b[j] {
                lcs[at(i + 1, j + 1)] + 1
            } else {
                lcs[at(i + 1, j)].max(lcs[at(i, j + 1)])
            };
        }
    }
    let (mut i, mut j, mut ops) = (0, 0, Vec::with_capacity(n + m));
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push(Op::Equal(a[i]));
            (i, j) = (i + 1, j + 1);
        } else if i < n && (j == m || lcs[at(i + 1, j)] >= lcs[at(i, j + 1)]) {
            ops.push(Op::Delete(a[i]));
            i += 1;
        } else {
            ops.push(Op::Insert(b[j]));
            j += 1;
        }
    }
    ops
}

/// `-`/`+` lines with `context` unchanged lines around each change; empty when equal.
pub fn render(ops: &[Op], context: usize) -> String {
    let changed: Vec<usize> = (0..ops.len())
        .filter(|&i| !matches!(ops[i], Op::Equal(_)))
        .collect();
    let near = |i: usize| {
        changed
            .binary_search_by(|&c| {
                if c + context < i {
                    std::cmp::Ordering::Less
                } else if c > i + context {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    };
    let mut out = String::new();
    let mut skipped = false;
    for (i, op) in ops.iter().enumerate() {
        if !near(i) {
            skipped = true;
            continue;
        }
        if skipped && !out.is_empty() {
            out.push_str("   …\n");
        }
        skipped = false;
        let (mark, line) = match op {
            Op::Equal(l) => (' ', l),
            Op::Delete(l) => ('-', l),
            Op::Insert(l) => ('+', l),
        };
        out.push_str(&format!(" {mark} {line}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_minimal_edit() {
        let ops = lines("a\nb\nc\nd", "a\nx\nc\nd\ne");
        assert_eq!(
            ops,
            [
                Op::Equal("a"),
                Op::Delete("b"),
                Op::Insert("x"),
                Op::Equal("c"),
                Op::Equal("d"),
                Op::Insert("e"),
            ]
        );
    }

    #[test]
    fn renders_changes_with_context() {
        let a = (0..20)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let b = a.replace("10", "ten");
        assert_eq!(render(&lines(&a, &b), 1), "   9\n - 10\n + ten\n   11\n");
        assert_eq!(render(&lines(&a, &a), 3), "");
        let c = b.replacen("2\n", "two\n", 1);
        assert_eq!(
            render(&lines(&a, &c), 0),
            " - 2\n + two\n   …\n - 10\n + ten\n"
        );
    }
}
