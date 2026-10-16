//! The install baseline kept on disk next to the compile cache (`baseline_cache.rs`): the second run of the same
//! program reads it instead of running the install code, and anything that could make the baseline another one
//! makes another file, so a test never sees data from a program that is not the one it runs against.

use al_sema::workspace::AppSource;
use al_testrun::Outcome;
use al_testrun::compile_cache::CompileCache;
use al_testrun::runtime::{CorpusRuntime, Manifest, RunOptions, SuiteRun};
use std::path::Path;

fn base(value: &str) -> String {
    format!(
        r#"
table 50100 Setting {{ fields {{ field(1; Code; Code[20]) {{ }} field(2; Value; Text[50]) {{ }} }} keys {{ key(PK; Code) {{ }} }} }}
codeunit 50101 Installer
{{
    Subtype = Install;
    trigger OnInstallAppPerCompany() var S: Record Setting; begin S.Code := 'SEEDED'; S.Value := '{value}'; S.Insert(); end;
}}
"#
    )
}

fn main_app(expected: &str) -> String {
    format!(
        r#"
codeunit 60101 DataTests
{{
    Subtype = Test;
    [Test] procedure ReadsTheInstallRow() var S: Record Setting; begin S.Get('SEEDED'); if S.Value <> '{expected}' then Error('got ' + S.Value); end;
}}
"#
    )
}

fn app(id: &str, deps: &[&str], src: String) -> AppSource {
    AppSource {
        id: id.into(),
        name: id.into(),
        dependencies: deps.iter().map(|d| d.to_string()).collect(),
        files: vec![(format!("{id}.al"), src)],
    }
}

fn run(installed: &str, expected: &str, cache: Option<&Path>) -> CorpusRuntime {
    let manifest = |p: &str| Manifest {
        publisher: p.into(),
        version: "1.0.0.0".into(),
        resources: vec![],
        include_source: false,
        symbols: vec![],
    };
    CorpusRuntime::try_run_suites_with(
        vec![
            app("main", &["base"], main_app(expected)),
            app("base", &[], base(installed)),
        ],
        vec![al_sema::AppKind::Workspace, al_sema::AppKind::Dependency],
        &[manifest("Me"), manifest("Microsoft")],
        &Default::default(),
        None,
        Default::default(),
        &[SuiteRun::everything()],
        &RunOptions {
            compile_cache: cache.map(|dir| CompileCache {
                dir: dir.to_path_buf(),
                require: false,
            }),
            ..Default::default()
        },
    )
    .expect("the run starts")
}

fn outcome(rt: &CorpusRuntime) -> Outcome {
    rt.results()
        .iter()
        .find(|r| r.test == "ReadsTheInstallRow")
        .map(|r| r.outcome.clone())
        .expect("the test ran")
}

fn files(cache: &Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(al_testrun::baseline_cache::folder(cache))
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn the_second_run_of_a_program_reads_the_baseline_the_first_one_kept() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(outcome(&run("yes", "yes", Some(dir.path()))), Outcome::Pass);
    let kept = files(dir.path());
    assert_eq!(kept.len(), 1, "the first run keeps its baseline");
    let (bytes, modified) = (
        std::fs::read(&kept[0]).unwrap(),
        std::fs::metadata(&kept[0]).unwrap().modified().unwrap(),
    );
    assert_eq!(outcome(&run("yes", "yes", Some(dir.path()))), Outcome::Pass);
    assert_eq!(files(dir.path()), kept, "no second file");
    assert_eq!(
        std::fs::read(&kept[0]).unwrap(),
        bytes,
        "the file is read, not written again"
    );
    assert_eq!(std::fs::metadata(&kept[0]).unwrap().modified().unwrap(), modified);
}

#[test]
fn a_changed_install_codeunit_does_not_get_the_old_baseline() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(outcome(&run("yes", "yes", Some(dir.path()))), Outcome::Pass);
    // The same test against an install code that writes another value. The baseline of the first run holds `yes`: a
    // run that took it would pass.
    assert!(
        matches!(outcome(&run("no", "yes", Some(dir.path()))), Outcome::Fail(_)),
        "the row is the one the new install code wrote"
    );
    assert_eq!(files(dir.path()).len(), 2, "another program, another file");
    // The first program still finds its own.
    assert_eq!(outcome(&run("yes", "yes", Some(dir.path()))), Outcome::Pass);
    assert_eq!(files(dir.path()).len(), 2);
}

#[test]
fn a_changed_workspace_app_gets_a_baseline_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    run("yes", "yes", Some(dir.path()));
    run("yes", "other", Some(dir.path()));
    assert_eq!(files(dir.path()).len(), 2, "the workspace's source is part of the key");
}

#[test]
fn a_run_without_the_compile_cache_keeps_nothing() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(outcome(&run("yes", "yes", None)), Outcome::Pass);
    assert!(files(dir.path()).is_empty());
}
