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
