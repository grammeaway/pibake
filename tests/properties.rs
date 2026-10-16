use std::path::{Component, Path};

use hegel::TestCase;
use hegel::generators as gs;
use pibake::package::{Filter, PackageEntry, Source};
use pibake::paths;

/// Source strings across every form Pi distinguishes, plus arbitrary text.
fn draw_source(tc: &TestCase) -> String {
    match tc.draw(gs::integers::<u8>().max_value(4)) {
        0 => format!(
            "npm:{}",
            tc.draw(gs::from_regex(
                r"(@[a-z0-9-]{1,8}/)?[a-z0-9.-]{1,12}(@[\^~]?[0-9.]{1,8})?"
            ))
        ),
        1 => format!(
            "git:{}",
            tc.draw(gs::from_regex(
                r"[a-z.]{1,8}(/([A-Za-z0-9_-]{1,8}|\.|\.\.)){1,4}(\.git)?(@[a-z0-9]{1,8})?"
            ))
        ),
        2 => tc.draw(gs::from_regex(r"(\./|\.\./|~/|/)?[a-z0-9_/-]{1,16}")),
        _ => tc.draw(gs::text().max_size(24)),
    }
}

fn draw_patterns(tc: &TestCase) -> Option<Vec<String>> {
    tc.draw(gs::optional(gs::vecs(gs::text().max_size(12)).max_size(4)))
}

fn draw_entry(tc: &TestCase) -> PackageEntry {
    let source = draw_source(tc);
    let filter = tc.draw(gs::booleans()).then(|| Filter {
        extensions: draw_patterns(tc),
        skills: draw_patterns(tc),
        prompts: draw_patterns(tc),
        themes: draw_patterns(tc),
        autoload: tc.draw(gs::optional(gs::booleans())),
    });
    PackageEntry { source, filter }
}

#[hegel::test(test_cases = 500)]
fn package_entry_round_trips_through_pi_json(tc: TestCase) {
    let entry = draw_entry(&tc);
    let json = entry.to_json();
    let reparsed = PackageEntry::from_json(&json).expect("rendered entry parses");
    assert_eq!(reparsed, entry);
    // And through text, as it sits in settings.json.
    let text = serde_json::to_string(&json).unwrap();
    let from_text = PackageEntry::from_json(&serde_json::from_str(&text).unwrap()).unwrap();
    assert_eq!(from_text, entry);
}

#[hegel::test(test_cases = 500)]
fn source_parsing_is_total_and_deterministic(tc: TestCase) {
    let source = draw_source(&tc);
    let home = Path::new("/home/u");
    let agent = Path::new("/agent");
    let first = Source::parse(&source);
    let second = Source::parse(&source);
    assert_eq!(format!("{first:?}"), format!("{second:?}"));
    if let Ok(parsed) = first {
        // Install paths never escape their root: Pi refuses those, so must we.
        let install = paths::normalize(&parsed.install_path(agent, home));
        match parsed {
            Source::Npm { .. } => assert!(
                install.starts_with("/agent/npm/node_modules"),
                "{install:?}"
            ),
            Source::Git { .. } => assert!(install.starts_with("/agent/git"), "{install:?}"),
            Source::Local { .. } => {}
        }
    }
}

#[hegel::test(test_cases = 300)]
fn npm_spec_splits_back_into_itself(tc: TestCase) {
    let spec = tc.draw(gs::from_regex(r"@?[a-z0-9/@.^~-]{0,16}"));
    let Ok(Source::Npm {
        spec: parsed,
        name,
        version,
    }) = Source::parse(&format!("npm:{spec}"))
    else {
        panic!("npm: prefix always parses as npm");
    };
    assert_eq!(parsed, spec);
    match version {
        Some(v) => {
            assert!(!v.is_empty(), "Pi's regex needs a non-empty version");
            assert_eq!(format!("{name}@{v}"), spec);
        }
        None => assert_eq!(name, spec),
    }
}

#[hegel::test(test_cases = 300)]
fn identity_ignores_version_ref_and_url_form(tc: TestCase) {
    let home = Path::new("/home/u");
    let agent = Path::new("/agent");
    let host = tc.draw(gs::from_regex(r"[a-z]{1,8}\.(com|org|dev)"));
    let owner = tc.draw(gs::from_regex(r"[A-Za-z0-9_-]{1,8}"));
    let repo = tc.draw(gs::from_regex(r"[A-Za-z0-9_-]{1,8}"));
    let git_ref = tc.draw(gs::optional(gs::from_regex(r"[a-z0-9.]{1,8}")));
    let at_ref = git_ref.map(|r| format!("@{r}")).unwrap_or_default();
    let forms = [
        format!("git:{host}/{owner}/{repo}{at_ref}"),
        format!("https://{host}/{owner}/{repo}.git{at_ref}"),
        format!("git:git@{host}:{owner}/{repo}{at_ref}"),
        format!("ssh://git@{host}/{owner}/{repo}{at_ref}"),
    ];
    for form in forms {
        let parsed = Source::parse(&form).unwrap_or_else(|e| panic!("{form}: {e}"));
        assert_eq!(
            parsed.identity(agent, home),
            format!("git:{host}/{owner}/{repo}"),
            "{form}"
        );
    }

    let name = tc.draw(gs::from_regex(r"(@[a-z]{1,6}/)?[a-z][a-z0-9-]{0,10}"));
    let version = tc.draw(gs::from_regex(r"[\^~]?[0-9]{1,2}(\.[0-9]{1,2}){0,2}"));
    for spec in [name.clone(), format!("{name}@{version}")] {
        let parsed = Source::parse(&format!("npm:{spec}")).unwrap();
        assert_eq!(parsed.identity(agent, home), format!("npm:{name}"));
    }
}

#[hegel::test(test_cases = 300)]
fn resolved_paths_are_normal_and_normalising_is_idempotent(tc: TestCase) {
    let input = tc.draw(gs::from_regex(
        r"(\./|\.\./|/|~/)?([a-z.]{1,3}/){0,5}[a-z.]{0,3}",
    ));
    let resolved = paths::resolve(Path::new("/base/dir"), &input, Path::new("/home/u"));
    assert!(resolved.is_absolute());
    assert!(
        resolved
            .components()
            .all(|c| !matches!(c, Component::CurDir | Component::ParentDir)),
        "{resolved:?}"
    );
    assert_eq!(paths::normalize(&resolved), resolved);
}

// --- l2: harness components ---

use pibake::bench::{Summary, utc_date};
use pibake::diff::{self, Op};
use pibake::fake_provider::chunk_str;
use pibake::snapshot::Normalizer;
use std::path::PathBuf;

/// Lines drawn from a small alphabet, so the two sides share lines often.
fn draw_lines(tc: &TestCase) -> String {
    tc.draw(gs::vecs(gs::sampled_from(vec!["a", "b", "c", "", " d"])).max_size(30))
        .join("\n")
}

#[hegel::test(test_cases = 500)]
fn line_diff_rebuilds_both_sides(tc: TestCase) {
    let (a, b) = (draw_lines(&tc), draw_lines(&tc));
    let ops = diff::lines(&a, &b);
    let old: Vec<&str> = ops
        .iter()
        .filter_map(|op| match *op {
            Op::Equal(l) | Op::Delete(l) => Some(l),
            Op::Insert(_) => None,
        })
        .collect();
    let new: Vec<&str> = ops
        .iter()
        .filter_map(|op| match *op {
            Op::Equal(l) | Op::Insert(l) => Some(l),
            Op::Delete(_) => None,
        })
        .collect();
    assert_eq!(old, a.lines().collect::<Vec<_>>());
    assert_eq!(new, b.lines().collect::<Vec<_>>());
    // An equal pair of sides diffs to nothing, and only then.
    assert_eq!(diff::render(&ops, 2).is_empty(), a.lines().eq(b.lines()));
}

#[hegel::test(test_cases = 500)]
fn chunks_rejoin_and_fit(tc: TestCase) {
    let text = tc.draw(gs::text().max_size(200));
    let max = tc.draw(gs::integers::<usize>().min_value(4).max_value(40));
    let chunks = chunk_str(&text, max);
    assert_eq!(chunks.concat(), text);
    assert!(chunks.iter().all(|c| !c.is_empty() && c.len() <= max));
}

#[hegel::test(test_cases = 500)]
fn normalizing_is_idempotent_and_leaves_no_rule_path(tc: TestCase) {
    let paths = [
        "/t/pibake-q/home/.pi/agent",
        "/t/pibake-q/home",
        "/t/pibake-q/work",
        "/t/pibake-q",
    ];
    let names = ["agent_dir", "home", "cwd", "harness"];
    let normalizer = Normalizer::new(paths.iter().map(PathBuf::from).zip(names));
    let count = tc.draw(gs::integers::<usize>().max_value(12));
    let text: String = (0..count)
        .map(|_| {
            if tc.draw(gs::booleans()) {
                tc.draw(gs::sampled_from(paths.to_vec())).to_string()
            } else {
                tc.draw(gs::from_regex(r"[a-z/._ <>-]{0,6}"))
            }
        })
        .collect();
    let once = normalizer.text(&text);
    assert_eq!(normalizer.text(&once), once);
    for path in paths {
        for (i, _) in once.match_indices(path) {
            let next = once[i + path.len()..].chars().next();
            assert!(
                next.is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_'),
                "{path} left whole in {once:?}"
            );
        }
    }
}

#[hegel::test(test_cases = 300)]
fn summary_is_ordered_and_order_free(tc: TestCase) {
    let samples = tc.draw(
        gs::vecs(
            gs::floats::<f64>()
                .allow_nan(false)
                .allow_infinity(false)
                .min_value(-1e9)
                .max_value(1e9),
        )
        .min_size(1)
        .max_size(30),
    );
    let s = Summary::of(&samples).unwrap();
    assert!(
        s.min <= s.p25 && s.p25 <= s.median && s.median <= s.p75 && s.p75 <= s.max,
        "{s:?}"
    );
    let mut reversed = samples.clone();
    reversed.reverse();
    assert_eq!(Summary::of(&reversed).unwrap(), s);
    assert_eq!(s.n, samples.len());
}

#[hegel::test(test_cases = 100)]
fn utc_date_matches_gnu_date(tc: TestCase) {
    let secs = tc.draw(gs::integers::<u64>().max_value(253_402_300_799)); // through 9999-12-31
    let out = std::process::Command::new("date")
        .args(["-u", "-d", &format!("@{secs}"), "+%Y-%m-%d"])
        .output()
        .unwrap();
    assert_eq!(
        utc_date(secs),
        String::from_utf8(out.stdout).unwrap().trim(),
        "{secs}"
    );
}

// --- l3: lock ---

use pibake::lock::{self, GitPin, Lock, NodePin, NpmLockPin, PiPin, Setup};
use std::collections::BTreeMap;

fn draw_lock(tc: &TestCase) -> Lock {
    let text = || tc.draw(gs::text().max_size(20));
    let git = (0..tc.draw(gs::integers::<usize>().max_value(4)))
        .map(|_| GitPin {
            name: text(),
            url: text(),
            commit: text(),
        })
        .collect();
    Lock {
        pi: PiPin {
            version: text(),
            integrity: text(),
        },
        node: NodePin { version: text() },
        npm_lock: NpmLockPin { sha256: text() },
        git,
    }
}

#[hegel::test(test_cases = 500)]
fn lock_render_parse_render_is_byte_identical(tc: TestCase) {
    let lock = draw_lock(&tc);
    let text = lock.render();
    let parsed = Lock::parse(&text).unwrap_or_else(|e| panic!("{e:#}\n{text}"));
    assert_eq!(parsed.render(), text);
    let mut sorted = lock.clone();
    sorted.git.sort();
    assert_eq!(parsed, sorted);
    assert!(text.ends_with('\n'));
}

/// Setups over tiny alphabets, so two draws are often equal or differ in one place.
fn draw_setup(tc: &TestCase) -> Setup {
    let pick = |options: &[&str]| tc.draw(gs::sampled_from(options.to_vec())).to_owned();
    let mut npm = BTreeMap::new();
    for _ in 0..tc.draw(gs::integers::<usize>().max_value(2)) {
        npm.insert(pick(&["a", "@s/b"]), pick(&["1.0.0", "1.0.1"]));
    }
    let mut git = BTreeMap::new();
    for _ in 0..tc.draw(gs::integers::<usize>().max_value(2)) {
        git.insert(
            pick(&["h.org/o/r", "h.org/o/s"]),
            (
                pick(&["https://h.org/o/r", "git@h.org:o/r"]),
                pick(&["c1", "c2"]),
            ),
        );
    }
    Setup {
        pi_version: pick(&["1.3.0", "1.4.0"]),
        node_version: pick(&["26.11.1", "26.12.0"]),
        npm,
        git,
    }
}

#[hegel::test(test_cases = 1000)]
fn drift_is_empty_exactly_when_the_setup_is_unchanged(tc: TestCase) {
    let (locked, now) = (draw_setup(&tc), draw_setup(&tc));
    let lock = Lock::pin(&locked, "sha512-x".into(), "00".into());
    let drift = lock::drift(&lock, &locked.npm, &now);
    assert_eq!(drift.is_empty(), locked == now, "{drift:?}");
    // Each differing package is named once.
    let changed_npm = locked
        .npm
        .keys()
        .chain(now.npm.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|n| locked.npm.get(*n) != now.npm.get(*n))
        .count();
    assert_eq!(
        drift.iter().filter(|d| d.starts_with("npm ")).count(),
        changed_npm
    );
}

// --- l4: stage ---

mod common;

use pibake::stage::{Inputs, content_hash};

fn hash_of(files: &[(String, Vec<u8>)]) -> String {
    content_hash(files.iter().map(|(n, c)| (n.as_str(), c.as_slice())))
}

#[hegel::test(test_cases = 500)]
fn content_hash_is_order_free_and_sees_every_byte(tc: TestCase) {
    let files: Vec<(String, Vec<u8>)> = tc
        .draw(
            gs::btree_maps(gs::from_regex(r"[a-z/.]{0,6}"), gs::binary().max_size(16)).max_size(6),
        )
        .into_iter()
        .collect();
    let hash = hash_of(&files);
    assert_eq!(hash_of(&tc.draw(gs::permutations(files.clone()))), hash);

    let mut changed = files.clone();
    let i = tc.draw(gs::integers::<usize>().max_value(files.len()));
    match changed.get_mut(i) {
        // A new file, possibly empty.
        None => changed.push(("new/".into(), tc.draw(gs::binary().max_size(4)))),
        Some((_, content)) if !content.is_empty() => {
            let at = tc.draw(gs::integers::<usize>().max_value(content.len() - 1));
            content[at] ^= tc.draw(gs::integers::<u8>().min_value(1));
        }
        Some((_, content)) => content.push(tc.draw(gs::integers::<u8>())),
    }
    assert_ne!(hash_of(&changed), hash);
}

#[hegel::test(test_cases = 100)]
fn stage_hash_follows_extension_bytes_not_creation_order(tc: TestCase) {
    let files: Vec<(String, Vec<u8>)> = tc
        .draw(
            gs::btree_maps(
                gs::from_regex(r"[a-z]{1,3}(/[a-z]{1,3}){0,2}\.ts"),
                gs::binary().min_size(1).max_size(16),
            )
            .min_size(1)
            .max_size(6),
        )
        .into_iter()
        .collect();
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    common::write_lock_files(&project, &common::sample_npm_lock(), vec![]);
    let agent = |name: &str, files: &[(String, Vec<u8>)]| -> String {
        let dir = root.path().join(name);
        for (rel, content) in files {
            let path = dir.join("extensions").join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let inventory = pibake::inventory::read(&dir, Path::new("/nonexistent-home")).unwrap();
        Inputs::read(&project, &inventory)
            .unwrap()
            .hash()
            .to_owned()
    };
    let hash = agent("a", &files);
    assert_eq!(agent("b", &tc.draw(gs::permutations(files.clone()))), hash);

    let mut changed = files.clone();
    let (i, at) = (
        tc.draw(gs::integers::<usize>().max_value(files.len() - 1)),
        tc.draw(gs::integers::<usize>()),
    );
    let content = &mut changed[i].1;
    let at = at % content.len();
    content[at] ^= tc.draw(gs::integers::<u8>().min_value(1));
    assert_ne!(agent("c", &changed), hash);
}

/// A random import graph over a few files, some of them in Pi's packages.
fn draw_metafile(tc: &TestCase) -> (Vec<String>, pibake::bundle::Metafile) {
    let names: Vec<String> = (0..8)
        .map(|i| {
            if i % 3 == 2 {
                format!("node_modules/@earendil-works/pi-ai/f{i}.js")
            } else {
                format!("node_modules/p{i}/i.js")
            }
        })
        .collect();
    let edges = tc.draw(gs::vecs(gs::tuples2(
        gs::integers::<usize>().max_value(7),
        gs::integers::<usize>().max_value(7),
    )));
    let mut inputs = std::collections::BTreeMap::<String, pibake::bundle::Input>::new();
    for (from, to) in edges {
        inputs
            .entry(names[from].clone())
            .or_default()
            .imports
            .push(pibake::bundle::Import {
                path: names[to].clone(),
            });
    }
    (names, pibake::bundle::Metafile { inputs })
}

#[hegel::test(test_cases = 300)]
fn reach_is_the_closure_outside_pi(tc: TestCase) {
    let (names, metafile) = draw_metafile(&tc);
    let start = &names[tc.draw(gs::integers::<usize>().max_value(7))];
    let is_pi = |f: &str| f.starts_with("node_modules/@earendil-works/");
    // Fixed point: add every non-Pi target of an edge out of the set.
    let mut closure = std::collections::BTreeSet::new();
    if !is_pi(start) {
        closure.insert(start.clone());
    }
    loop {
        let before = closure.len();
        for (from, input) in &metafile.inputs {
            if closure.contains(from) {
                closure.extend(
                    input
                        .imports
                        .iter()
                        .map(|i| i.path.clone())
                        .filter(|p| !is_pi(p)),
                );
            }
        }
        if closure.len() == before {
            break;
        }
    }
    assert_eq!(pibake::bundle::reach(&metafile, start), closure);
}

#[hegel::test(test_cases = 300)]
fn every_warning_is_reported_once_for_pi_or_for_the_extensions_reaching_it(tc: TestCase) {
    use pibake::bundle::{Baked, BuildResult, Message, reach, report};
    let (names, metafile) = draw_metafile(&tc);
    let entries = tc.draw(gs::vecs(gs::integers::<usize>().max_value(7)).max_size(4));
    let baked: Vec<Baked> = entries
        .iter()
        .enumerate()
        .map(|(i, &n)| Baked {
            owner: format!("o{i}"),
            key: format!("k{i}"),
            stage_path: names[n].clone(),
        })
        .collect();
    let warnings: Vec<Message> = tc
        .draw(gs::vecs(gs::integers::<usize>().max_value(7)).max_size(6))
        .into_iter()
        .enumerate()
        .map(|(i, n)| Message {
            text: format!("w{i}"),
            id: None,
            file: Some(names[n].clone()),
            line: None,
        })
        .collect();
    let result = BuildResult {
        warnings: warnings.clone(),
        metafile: Some(metafile.clone()),
        ..Default::default()
    };
    let r = report(&baked, &Default::default(), &result, vec![]);
    for w in &warnings {
        let file = w.file.as_ref().unwrap();
        let in_pi = r.pi_warnings.contains(w);
        for (b, e) in baked.iter().zip(&r.extensions) {
            assert_eq!(
                e.warnings.contains(w),
                reach(&metafile, &b.stage_path).contains(file),
                "{w:?} for {}",
                b.stage_path
            );
        }
        let in_any = r.extensions.iter().any(|e| e.warnings.contains(w));
        assert!(in_pi != in_any, "{w:?}: pi {in_pi}, extensions {in_any}");
    }
}

/// `install` writes `~/.local/bin/<name>`: an accepted name is one plain path component.
#[hegel::test(test_cases = 500)]
fn build_names_stay_one_file_name(tc: TestCase) {
    let name = match tc.draw(gs::integers::<u8>().max_value(2)) {
        0 => tc.draw(gs::from_regex(r"(\.{0,2}/)?[a-z.]{0,4}(/[a-z.]{0,4})?/?")),
        _ => tc.draw(gs::text().max_size(12)),
    };
    let toml = format!("[build]\nname = {}", toml::Value::String(name.clone()));
    let Ok(config) = pibake::config::Config::parse(&toml, Path::new("/p"), Path::new("/h")) else {
        return;
    };
    assert_eq!(config.name, name);
    let components: Vec<_> = Path::new(&name).components().collect();
    assert!(
        matches!(components[..], [Component::Normal(c)] if c == name.as_str()),
        "{name:?} -> {components:?}"
    );
}
