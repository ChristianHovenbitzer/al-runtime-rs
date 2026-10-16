//! `CorpusRuntime`: compiling suites of apps and running them.
use super::*;

impl CorpusRuntime {
    pub fn run(apps: Vec<al_sema::workspace::AppSource>, symbols: &BTreeSet<String>) -> CorpusRuntime {
        Self::run_with_manifests(apps, &[], symbols)
    }

    /// `manifests[i]` belongs to `apps[i]`: the publisher and version `ModuleInfo` reports.
    pub fn run_with_manifests(
        apps: Vec<al_sema::workspace::AppSource>,
        manifests: &[Manifest],
        symbols: &BTreeSet<String>,
    ) -> CorpusRuntime {
        Self::run_with_dotnet(apps, manifests, symbols, al_vm::dotnet::DotNetConfig::default())
    }

    /// [`run_with_manifests`](Self::run_with_manifests) with a `DotNet` host (ADR-014); the
    /// default has none.
    pub fn run_with_dotnet(
        apps: Vec<al_sema::workspace::AppSource>,
        manifests: &[Manifest],
        symbols: &BTreeSet<String>,
        dotnet: al_vm::dotnet::DotNetConfig,
    ) -> CorpusRuntime {
        Self::run_for(apps, manifests, symbols, None, dotnet)
    }

    /// Runs for a target: builtins newer than the target's runtime are refused and version gates
    /// follow it (ADR-015). Without a target, the newest catalog and behaviour are used.
    pub fn run_for(
        apps: Vec<al_sema::workspace::AppSource>,
        manifests: &[Manifest],
        symbols: &BTreeSet<String>,
        target: Option<&al_version::Target>,
        dotnet: al_vm::dotnet::DotNetConfig,
    ) -> CorpusRuntime {
        let kinds = vec![al_sema::AppKind::Workspace; apps.len()];
        Self::run_for_apps(apps, kinds, manifests, symbols, target, dotnet)
    }

    /// [`run_for`](Self::run_for) with the kind of each app. Dependency apps are compiled but no
    /// test is run in them (ADR-013 decision 4).
    pub fn run_for_apps(
        apps: Vec<al_sema::workspace::AppSource>,
        kinds: Vec<al_sema::AppKind>,
        manifests: &[Manifest],
        symbols: &BTreeSet<String>,
        target: Option<&al_version::Target>,
        dotnet: al_vm::dotnet::DotNetConfig,
    ) -> CorpusRuntime {
        Self::run_suites(
            apps,
            kinds,
            manifests,
            symbols,
            target,
            dotnet,
            &[SuiteRun::everything()],
        )
    }

    /// [`run_for_apps`](Self::run_for_apps) for several runs over the same compiled apps: they
    /// share one compile and one install baseline, and each test codeunit of a run is an isolation
    /// unit on a session of its own (`isolation.rs`).
    pub fn run_suites(
        apps: Vec<al_sema::workspace::AppSource>,
        kinds: Vec<al_sema::AppKind>,
        manifests: &[Manifest],
        symbols: &BTreeSet<String>,
        target: Option<&al_version::Target>,
        dotnet: al_vm::dotnet::DotNetConfig,
        suites: &[SuiteRun],
    ) -> CorpusRuntime {
        Self::run_suites_with(
            apps,
            kinds,
            manifests,
            symbols,
            target,
            dotnet,
            suites,
            &RunOptions::default(),
        )
    }

    /// [`run_suites`](Self::run_suites) with a test selection and tracing (`RunOptions`).
    #[allow(clippy::too_many_arguments)]
    pub fn run_suites_with(
        apps: Vec<al_sema::workspace::AppSource>,
        kinds: Vec<al_sema::AppKind>,
        manifests: &[Manifest],
        symbols: &BTreeSet<String>,
        target: Option<&al_version::Target>,
        dotnet: al_vm::dotnet::DotNetConfig,
        suites: &[SuiteRun],
        opts: &RunOptions,
    ) -> CorpusRuntime {
        Self::try_run_suites_with(apps, kinds, manifests, symbols, target, dotnet, suites, opts)
            .expect("a run without `require` does not fail to start")
    }

    /// [`run_suites_with`](Self::run_suites_with), which says why a run could not start: the
    /// compile cache is required (`CompileCache::require`) and has no usable entry.
    #[allow(clippy::too_many_arguments)]
    pub fn try_run_suites_with(
        apps: Vec<al_sema::workspace::AppSource>,
        kinds: Vec<al_sema::AppKind>,
        manifests: &[Manifest],
        symbols: &BTreeSet<String>,
        target: Option<&al_version::Target>,
        dotnet: al_vm::dotnet::DotNetConfig,
        suites: &[SuiteRun],
        opts: &RunOptions,
    ) -> Result<CorpusRuntime, String> {
        let compiled = Self::compile_suites(apps, kinds, manifests, symbols, target, suites, opts)?;
        let rt = Self::run_compiled(&compiled, target, dotnet, opts);
        // The workspace and the lowered program are big: dropping them takes as long as lowering
        // a tenth of the corpus, and nothing waits for it.
        retire(compiled);
        Ok(rt)
    }

    /// The compile phase of a run: the workspace, the lowered program and what the run phase needs
    /// to know about it ([`run_compiled`](Self::run_compiled)). A warm server keeps this between requests.
    #[allow(clippy::too_many_arguments)]
    pub fn compile_suites(
        apps: Vec<al_sema::workspace::AppSource>,
        kinds: Vec<al_sema::AppKind>,
        manifests: &[Manifest],
        symbols: &BTreeSet<String>,
        target: Option<&al_version::Target>,
        suites: &[SuiteRun],
        opts: &RunOptions,
    ) -> Result<Compiled, String> {
        // AL_NO_BASELINE=1 leaves the install baseline out (to measure what it costs).
        let baseline_wanted =
            kinds.contains(&al_sema::AppKind::Dependency) && std::env::var_os("AL_NO_BASELINE").is_none();
        // The baseline runs only when a selected test can observe it (`observe.rs`).
        // AL_FORCE_BASELINE=1 runs it always: the other side of the differential check, which
        // must show no outcome that changes. A run with a custom test runner keeps it too.
        let try_skip = baseline_wanted
            && std::env::var_os("AL_FORCE_BASELINE").is_none()
            && !opts.execute
            && suites.iter().all(|s| s.runner.is_none());
        let compile_started = std::time::Instant::now();
        // The compile cache holds the dependency apps, which come first in the program (ADR-022).
        // The caller's order is kept in `order` (program app -> caller app) for what it gets back.
        let use_cache = opts.compile_cache.is_some()
            && kinds.contains(&al_sema::AppKind::Dependency)
            && std::env::var_os("AL_EAGER_DEPENDENCIES").is_none();
        let n_apps = apps.len();
        let (apps, mut kinds, manifests, suites_in, only, order) = if use_cache {
            // Dependency apps first, Microsoft's before the others (the layers of the compile
            // cache follow this order), then the workspace's.
            let deps: Vec<usize> = (0..n_apps)
                .filter(|&i| kinds[i] == al_sema::AppKind::Dependency)
                .collect();
            let dep_apps: Vec<&al_sema::workspace::AppSource> = deps.iter().map(|&i| &apps[i]).collect();
            let dep_manifests: Vec<Manifest> = deps
                .iter()
                .map(|&i| manifests.get(i).cloned().unwrap_or_default())
                .collect();
            let order: Vec<usize> = crate::compile_cache::dependency_order(&dep_apps, &dep_manifests)
                .into_iter()
                .map(|k| deps[k])
                .chain((0..n_apps).filter(|&i| kinds[i] != al_sema::AppKind::Dependency))
                .collect();
            let mut new_of = vec![0; n_apps];
            for (new, &old) in order.iter().enumerate() {
                new_of[old] = new;
            }
            let mut slots: Vec<Option<al_sema::workspace::AppSource>> = apps.into_iter().map(Some).collect();
            let apps: Vec<_> = order.iter().map(|&i| slots[i].take().expect("each app once")).collect();
            let kinds: Vec<_> = order.iter().map(|&i| kinds[i]).collect();
            let manifests: Vec<Manifest> = order
                .iter()
                .map(|&i| manifests.get(i).cloned().unwrap_or_default())
                .collect();
            let suites: Vec<SuiteRun> = suites
                .iter()
                .map(|s| SuiteRun {
                    app: s.app.map(|a| new_of[a]),
                    ..s.clone()
                })
                .collect();
            let only = opts
                .only
                .as_ref()
                .map(|v| v.iter().map(|&(a, id)| (new_of[a], id)).collect::<Vec<_>>());
            (apps, kinds, manifests, suites, only, order)
        } else {
            (
                apps,
                kinds,
                manifests.to_vec(),
                suites.to_vec(),
                opts.only.clone(),
                (0..n_apps).collect(),
            )
        };
        let suites = &suites_in[..];
        let manifests = &manifests[..];
        // A dependency app is compiled with the symbols of its own app.json, the project's apps with the
        // project's (hard rule 6: a project that defines CLEAN25 does not change Base Application).
        let dep_symbols: Vec<Option<BTreeSet<String>>> = kinds
            .iter()
            .enumerate()
            .map(|(i, k)| {
                (*k == al_sema::AppKind::Dependency).then(|| {
                    manifests
                        .get(i)
                        .map(|m| m.symbols.iter().cloned().collect())
                        .unwrap_or_default()
                })
            })
            .collect();
        // The apps whose files hold no text yet, in program order.
        let lazy: Vec<Option<al_sema::LazySource>> =
            order.iter().map(|&i| opts.lazy.get(i).cloned().flatten()).collect();
        let identities: Vec<Option<[u8; 32]>> = lazy.iter().map(|l| l.as_ref().map(|l| l.identity)).collect();
        let any_lazy = lazy.iter().any(Option::is_some);
        let fill_all = |apps: &mut Vec<al_sema::workspace::AppSource>| {
            for (a, l) in apps.iter_mut().zip(&lazy) {
                if let Some(l) = l {
                    l.fill(a);
                }
            }
        };
        let roots = al_sema::check::Roots {
            baseline: baseline_wanted,
            only_tests: only.clone(),
            ..Default::default()
        };
        // Whether the install baseline runs, and why it does when it was weighed.
        let mut baseline = baseline_wanted;
        let mut baseline_note: Option<String> = None;
        let catalog = match target {
            Some(t) => al_builtins::catalog::Catalog::for_target(t),
            None => al_builtins::catalog::Catalog::load(),
        };
        let mut apps = apps;
        // Selected tests that look free of data are compiled first the way the demand-driven path
        // does it, without the baseline's roots and without the compile cache: that touches only what
        // the tests reach (a few routines), where the cache decodes every dependency object and the
        // demand-driven compile with the baseline reaches 421 routines of Base Application. If the rule
        // (`observe.rs`) then says the tests cannot observe the baseline, that is the compile. If it says
        // they may, the apps go on to the compile they would have had. `--require-compile-cache` (CI:
        // the measured run must be a hit) never takes this path.
        let mut compile_cache_skipped = false;
        let mut weighed = false;
        let mut compiled: Option<(al_sema::Workspace, Lowered)> = None;
        let require_cache = opts.compile_cache.as_ref().is_some_and(|c| c.require);
        let mut source_index: Option<&'static str> = None;
        if try_skip && !require_cache && crate::observe::text_looks_pure(&apps, &kinds, symbols, only.as_deref()) {
            let started = std::time::Instant::now();
            let kinds_again = kinds.clone();
            // The dependency apps that were read lazily are loaded from their index when the cache folder
            // has one: no source is read or scanned. Without one the sources are read, scanned, and the
            // index is saved for the next run.
            let cc = opts.compile_cache.as_ref();
            let (ws, filled) = match (any_lazy, cc) {
                (true, Some(cc)) => match lazy_index(cc, &apps, manifests, &lazy, &identities) {
                    Ok(deps) => {
                        source_index = Some("read");
                        (
                            al_sema::Workspace::load_apps_symbols(apps, kinds, symbols, &dep_symbols, deps),
                            false,
                        )
                    }
                    Err(key) => {
                        fill_all(&mut apps);
                        let ws = al_sema::Workspace::load_apps_symbols(apps, kinds, symbols, &dep_symbols, Vec::new());
                        save_index(cc, &ws, &lazy, &key);
                        source_index = Some("built");
                        (ws, true)
                    }
                },
                _ => {
                    fill_all(&mut apps);
                    (
                        al_sema::Workspace::load_apps_symbols(apps, kinds, symbols, &dep_symbols, Vec::new()),
                        true,
                    )
                }
            };
            let load = started.elapsed();
            // Without the baseline's roots, and without the workspace's procedures no selected test names:
            // a helper beside the tests that touches tables must not pull the dependency apps in.
            let first_roots = al_sema::check::Roots {
                baseline: false,
                reach_only: true,
                ..roots.clone()
            };
            let (mut ws, lowered) = al_sema::lower::compile_loaded(ws, load, &catalog, first_roots);
            let selected = selected_tests(&lowered, suites, &only);
            weighed = true;
            baseline_note = crate::observe::may_observe_baseline(&lowered, &selected);
            baseline = baseline_note.is_some();
            if baseline {
                // Back to the apps as they were; a file not read yet is read when the compile that
                // follows needs it.
                let _ = filled;
                apps = std::mem::take(&mut ws.apps);
                kinds = kinds_again;
                retire((ws, lowered));
            } else {
                compile_cache_skipped = use_cache;
                compiled = Some((ws, lowered));
                apps = Vec::new();
                kinds = Vec::new();
            }
        }
        let (ws, mut lowered, cache_report, layers) = match compiled {
            Some((ws, lowered)) => (ws, lowered, None, None),
            None => match opts.compile_cache.as_ref().filter(|_| use_cache) {
                Some(cc) => {
                    let (ws, lowered, report, layers) = compile_with_cache(
                        cc,
                        apps,
                        kinds,
                        manifests,
                        symbols,
                        target,
                        &lazy,
                        &identities,
                        roots.clone(),
                    )?;
                    if try_skip && !weighed {
                        let selected = selected_tests(&lowered, suites, &only);
                        baseline_note = crate::observe::may_observe_baseline(&lowered, &selected);
                        baseline = baseline_note.is_some();
                    }
                    (ws, lowered, report, layers)
                }
                None => {
                    fill_all(&mut apps);
                    let started = std::time::Instant::now();
                    let ws = al_sema::Workspace::load_apps_symbols(apps, kinds, symbols, &dep_symbols, Vec::new());
                    let load = started.elapsed();
                    // Without a weighed answer, weigh on the compiled program, which has the baseline's
                    // roots: the rule only looks at what the tests reach.
                    let (ws, lowered) = al_sema::lower::compile_loaded(ws, load, &catalog, roots.clone());
                    if try_skip && !weighed {
                        let selected = selected_tests(&lowered, suites, &only);
                        baseline_note = crate::observe::may_observe_baseline(&lowered, &selected);
                        baseline = baseline_note.is_some();
                    }
                    (ws, lowered, None, None)
                }
            },
        };
        let source_index = source_index.or(cache_report.as_ref().and_then(|c| c.source_index));
        // A dependency file that does not parse takes every object in it out of the compile, and the
        // tests that need one are refused as "outside the workspace". Say so, once, with the first lines.
        // A file whose second parse gave another result is a defect, not a property of the file (issue 15).
        for flake in ws.parse_flakes() {
            eprintln!("parse: WARNING {flake}");
        }
        let mut unparsed = ws.lazy_parse_problems();
        // The files the cache supplies were parsed when it was built.
        if let Some(c) = &cache_report {
            unparsed.extend(c.parse_problems.iter().cloned());
            unparsed.sort();
            unparsed.dedup();
        }
        if !unparsed.is_empty() {
            eprintln!(
                "parse: {} dependency file(s) did not parse; their objects are unusable and the tests that need them are not run",
                unparsed.len()
            );
            for (_, why) in unparsed.iter().take(10) {
                eprintln!("parse: {why}");
            }
            if unparsed.len() > 10 {
                eprintln!("parse: ... {} more", unparsed.len() - 10);
            }
        }
        // Code the AL compiler rejects (must-not-compile corpus, `alcodes.rs`). Said, not acted on: the
        // run goes on, so an error found here that BC would not give shows in the output, not as a refusal.
        if !lowered.al_diagnostics.is_empty() {
            eprintln!(
                "compile: {} error(s) the AL compiler would give for the workspace's code",
                lowered.al_diagnostics.len()
            );
            for d in lowered.al_diagnostics.iter().take(100) {
                eprintln!("compile: error {} {}: {}", d.code, d.file, d.message);
            }
        }
        if std::env::var_os("AL_REACH_REPORT").is_some() {
            eprintln!("{}", crate::reach::report(&ws, &lowered));
        }
        // Nothing else holds the program yet, so this does not copy it.
        for (app, m) in std::sync::Arc::make_mut(&mut lowered.program)
            .apps
            .iter_mut()
            .zip(manifests)
        {
            app.publisher = m.publisher.clone();
            app.version = m.version.clone();
            app.resources = m.resources.clone();
            app.include_source = m.include_source;
        }
        // The source lines of an app that includes its source in the symbols (the Code Coverage `Line`).
        {
            let program = std::sync::Arc::make_mut(&mut lowered.program);
            for sf in &mut program.sources {
                let Some(app) = program.apps.get(sf.app as usize).filter(|a| a.include_source) else {
                    continue;
                };
                let _ = app;
                if let Some((_, text)) = ws
                    .apps
                    .get(sf.app as usize)
                    .and_then(|a| a.files.iter().find(|(p, _)| *p == sf.path))
                {
                    sf.lines = text.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
                }
            }
        }
        let opts_only = only;
        let compile_time = compile_started.elapsed();
        Ok(Compiled {
            ws,
            lowered,
            cache_report,
            baseline_wanted,
            baseline,
            baseline_note,
            order,
            suites: suites_in,
            opts_only,
            compile_cache_skipped,
            source_index,
            compile_time,
            incr: layers.map(|layers| Incr {
                layers,
                roots,
                catalog,
                try_skip,
                symbols: symbols.clone(),
            }),
        })
    }

    /// The run phase: engine, install baseline, the test codeunits. Leaves `compiled` as it was, so a warm
    /// server runs it again for the next request.
    pub fn run_compiled(
        compiled: &Compiled,
        target: Option<&al_version::Target>,
        dotnet: al_vm::dotnet::DotNetConfig,
        opts: &RunOptions,
    ) -> CorpusRuntime {
        let Compiled {
            ws,
            lowered,
            cache_report,
            baseline_wanted,
            baseline,
            baseline_note,
            order,
            suites,
            opts_only,
            compile_cache_skipped,
            source_index,
            compile_time: compile_phase,
            incr: _,
        } = compiled;
        let (baseline_wanted, baseline, compile_cache_skipped, source_index) =
            (*baseline_wanted, *baseline, *compile_cache_skipped, *source_index);
        let baseline_note = baseline_note.clone();
        // The selection of this request, in program order: a warm compile may reach more than the request asks for.
        let opts_only = match &opts.only {
            Some(v) => {
                let mut new_of = vec![0; order.len()];
                for (new, &old) in order.iter().enumerate() {
                    new_of[old] = new;
                }
                Some(v.iter().map(|&(a, id)| (new_of[a], id)).collect::<Vec<_>>())
            }
            None => opts_only.clone(),
        };
        let suites = &suites[..];
        let blocked: Vec<Vec<Option<String>>> = lowered
            .unsupported
            .iter()
            .map(|o| o.iter().map(|r| r.first().cloned()).collect())
            .collect();
        let limit = opts.test_time_limit;
        let workers = if opts.trace { 1 } else { test_workers() };
        let baseline_skipped = baseline_wanted && !baseline;
        let engine_started = std::time::Instant::now();
        let make_engine = || {
            let mut engine = Engine::with_config(
                lowered.program.clone(),
                blocked.clone(),
                target.cloned(),
                dotnet.clone(),
            );
            global_triggers_find_unavailable(&mut engine, lowered);
            if let Some(e) = Arc::get_mut(&mut engine) {
                e.test_data = opts.test_data.clone();
            }
            engine
        };
        let mut engine = make_engine();
        let engine_time = engine_started.elapsed();
        let compile_time = *compile_phase + engine_time;
        let started = std::time::Instant::now();
        // With several test threads the tests get a second engine once the baseline has run (below).
        // It does not depend on the baseline, so it is built beside it, on another core.
        // The baseline of an earlier run of the same program, when the compile cache is on and nothing outside the
        // program can have changed what the install code reads (`baseline_key`).
        let baseline_key = if baseline {
            baseline_key(
                ws,
                lowered,
                cache_report.as_ref(),
                suites,
                opts_only.as_deref(),
                target,
                &dotnet,
                opts,
            )
        } else {
            None
        };
        let kept = baseline_key
            .as_ref()
            .zip(opts.compile_cache.as_ref())
            .and_then(|(key, cc)| crate::baseline_cache::read(&cc.dir, key));
        let from_cache = kept.is_some();
        let (baseline_run, mut spare_engine) = std::thread::scope(|scope| {
            let spare = (baseline && workers > 1)
                .then(|| {
                    std::thread::Builder::new()
                        .stack_size(64 << 20)
                        .spawn_scoped(scope, make_engine)
                        .ok()
                })
                .flatten();
            let run = if baseline {
                kept.or_else(|| crate::baseline::run(&engine, lowered))
            } else {
                None
            };
            (run, spare.map(|h| h.join().expect("the second engine is built")))
        });
        if let (false, Some(key), Some(cc), Some(run)) = (
            from_cache,
            baseline_key.as_ref(),
            opts.compile_cache.as_ref(),
            baseline_run.as_ref(),
        ) {
            crate::baseline_cache::write(&cc.dir, key, run);
        }
        // The tests run against an engine that knows which tables the install baseline could not
        // seed. The baseline itself ran before it knew; with several test threads, which each run
        // the baseline again (its data is not for sharing between threads), the engine that ran it
        // stays as it was and the tests get another.
        let unseeded: std::collections::HashSet<usize> = baseline_run
            .as_ref()
            .map(|b| b.unseeded.iter().map(|&t| t as usize).collect())
            .unwrap_or_default();
        let baseline_engine = (workers > 1 && baseline_run.is_some()).then(|| engine.clone());
        if let Some(b) = &baseline_run {
            if from_cache {
                eprintln!("baseline: read from the cache of an earlier run of this program");
            }
            for n in &b.notes {
                eprintln!("baseline: {n}");
            }
            for l in b.restored_lines() {
                eprintln!("baseline: {l}");
            }
            eprintln!(
                "baseline: {} of {} install roots ran to their end; {} tables hold rows, {} are unseeded, {:.2} s",
                b.ran,
                b.total,
                b.filled,
                b.unseeded.len(),
                started.elapsed().as_secs_f64()
            );
            if baseline_engine.is_some() {
                engine = spare_engine.take().unwrap_or_else(make_engine);
            }
            if let Some(e) = Arc::get_mut(&mut engine) {
                e.unseeded = unseeded;
            }
        }
        let baseline_time = started.elapsed();
        let baseline = baseline_run.map(|b| b.baseline);
        let tests_started = std::time::Instant::now();
        let mut outcomes = std::collections::HashMap::new();
        let mut marks_by_test: std::collections::HashMap<(usize, u32, String), Vec<Assumption>> =
            std::collections::HashMap::new();
        let mut times_by_case: std::collections::HashMap<(usize, u32, String), std::time::Duration> =
            Default::default();
        let mut details = std::collections::HashMap::new();
        let mut panics = 0;
        let mut codeunit_times: Vec<CodeunitTime> = Vec::new();
        // The test codeunits to run, in the order the report lists them.
        let mut units: Vec<Unit> = Vec::new();
        let order_arc: Arc<Vec<usize>> = Arc::new(order.clone());
        let mut list: Vec<TestResult> = Vec::new();
        for (si, suite) in suites.iter().enumerate() {
            if opts.execute {
                // A scratch run: the `OnRun` trigger of one codeunit of the suite's app.
                if let Some(oi) = on_run_target(lowered, suite.app) {
                    units.push(Unit {
                        si,
                        oi,
                        id: lowered.program.objects[oi].id.unwrap_or(0),
                        isolation: TestIsolation::default(),
                        without_baseline: false,
                        coverage: opts.coverage,
                        execute: true,
                        capture: opts.capture,
                        order: order_arc.clone(),
                        hooks: opts.hooks.clone(),
                        clock_start: None,
                        dual: false,
                    });
                }
                continue;
            }
            let isolation = match suite.runner {
                None => Ok(TestIsolation::default()),
                Some(r) => runner_isolation(lowered, r),
            };
            for (oi, o) in lowered.program.objects.iter().enumerate() {
                let is_test = is_selected_test(lowered, oi, suite, &opts_only);
                let Some(id) = o.id.filter(|_| is_test) else { continue };
                match &isolation {
                    Ok(i) => units.push(Unit {
                        si,
                        oi,
                        id,
                        isolation: opts.isolation.unwrap_or(*i),
                        without_baseline: false,
                        coverage: opts.coverage,
                        execute: false,
                        capture: opts.capture,
                        order: order_arc.clone(),
                        hooks: opts.hooks.clone(),
                        clock_start: opts.clock_starts.as_ref().and_then(|m| m.get(&id).copied()),
                        dual: opts.dual_run,
                    }),
                    Err(why) => {
                        for r in test_routines(o) {
                            outcomes.insert((si, id, r.name.to_lowercase()), Outcome::NotRun(why.clone()));
                            list.push(TestResult {
                                suite: si,
                                app: order[o.app as usize],
                                codeunit: id,
                                codeunit_name: o.name.clone(),
                                test: r.name.clone(),
                                outcome: Outcome::NotRun(why.clone()),
                                call_stack: String::new(),
                                assumptions: Vec::new(),
                                time: std::time::Duration::ZERO,
                                coverage: Vec::new(),
                                messages: Vec::new(),
                                captured: None,
                            });
                        }
                    }
                }
            }
        }
        // AL_CHECK_BASELINE_RULE=1: the verification of `observe.rs`. The baseline runs as usual, but
        // each test codeunit the rule calls free of it starts on an empty store. A run compared with a
        // normal one must show no outcome that differs (`tools/baseline_rule_check.sh`).
        if std::env::var_os("AL_CHECK_BASELINE_RULE").is_some() && baseline.is_some() {
            let list = std::env::var("AL_CHECK_BASELINE_RULE").is_ok_and(|v| v == "list");
            for u in &mut units {
                let why = crate::observe::may_observe_baseline(lowered, &[u.oi]);
                if let (true, Some(why)) = (list, &why) {
                    eprintln!(
                        "baseline rule check: {} may observe it: {why}",
                        lowered.program.objects[u.oi].name
                    );
                }
                u.without_baseline = why.is_none();
            }
            eprintln!(
                "baseline rule check: {} of {} test codeunits run without the baseline",
                units.iter().filter(|u| u.without_baseline).count(),
                units.len()
            );
        }
        let results: Vec<UnitResult> = if workers <= 1 || units.len() < 2 {
            units
                .iter()
                .map(|u| {
                    let mut observe = |name: &str, _: &Outcome, seen: &Observed| {
                        let mut d = explain::detail(
                            &lowered.program,
                            seen.session.fault_site(),
                            seen.error.map(|e| explain::ErrorInfo {
                                message: e.message.clone(),
                                code: e.code.clone(),
                                call_stack: e.call_stack.clone(),
                            }),
                        );
                        d.refused_in = seen
                            .refused_in
                            .map(|(o, r)| explain::describe(&lowered.program, o, r, al_ir::NO_POS));
                        details.insert((u.si, u.id, name.to_lowercase()), d);
                    };
                    let trace: Option<&mut Observer> = opts.trace.then_some(&mut observe);
                    run_unit(&engine, lowered, baseline.as_ref(), u, trace, limit)
                })
                .collect()
        } else {
            // Test codeunits run on a session each and do not share anything (the install
            // baseline is put back for each), so worker threads take them in turn. A worker
            // sets up its own copy of the baseline.
            let next = std::sync::atomic::AtomicUsize::new(0);
            let pull = |baseline: Option<&al_vm::Baseline>| {
                let mut done = Vec::new();
                loop {
                    let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(u) = units.get(k) else { return done };
                    done.push((k, run_unit(&engine, lowered, baseline, u, None, limit)));
                }
            };
            // Each test thread starts from its own copy of the baseline (a baseline holds `Rc`s and cannot be shared
            // between threads), and reading a snapshot is far cheaper than running the install code again.
            let snapshot: Option<Vec<u8>> = baseline_engine
                .is_some()
                .then(|| baseline.as_ref().and_then(al_vm::Baseline::to_bytes))
                .flatten();
            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for _ in 1..workers.min(units.len()) {
                    let spawned = std::thread::Builder::new()
                        .stack_size(TEST_STACK)
                        .spawn_scoped(scope, || {
                            // The baseline the first run left, as its snapshot says; only a baseline that has
                            // no snapshot (it reads test data) is run again, once per thread.
                            let own = baseline_engine.as_ref().and_then(|e| {
                                snapshot
                                    .as_deref()
                                    .and_then(al_vm::Baseline::from_bytes)
                                    .or_else(|| crate::baseline::run(e, lowered).map(|b| b.baseline))
                            });
                            pull(own.as_ref())
                        });
                    match spawned {
                        Ok(h) => handles.push(h),
                        // No room for another thread (an address space limit): fewer will do.
                        Err(_) => break,
                    }
                }
                let mut done = pull(baseline.as_ref());
                for h in handles {
                    done.extend(h.join().expect("a test worker panicked"));
                }
                // Results in the order of the units, whichever worker finished first.
                done.sort_by_key(|(k, _)| *k);
                done.into_iter().map(|(_, r)| r).collect()
            })
        };
        let mut suite_work: Vec<(usize, std::time::Duration)> = Vec::new();
        let mut dual_diffs: Vec<crate::dual::DualDiff> = Vec::new();
        for r in results {
            dual_diffs.extend(r.dual.iter().cloned());
            suite_work.push((r.si, r.took));
            codeunit_times.push(CodeunitTime {
                id: r.id,
                name: lowered.program.objects[r.oi].name.clone(),
                took: r.took,
                sidecar_start: r.sidecar_start,
            });
            let o = &lowered.program.objects[r.oi];
            let push = |list: &mut Vec<TestResult>,
                        test: &str,
                        outcome: &Outcome,
                        call_stack: String,
                        assumptions: Vec<Assumption>,
                        time: std::time::Duration,
                        coverage: Vec<(u32, u32)>,
                        messages: Vec<String>,
                        captured: Option<crate::events::CapturedAt>| {
                list.push(TestResult {
                    suite: r.si,
                    app: order[o.app as usize],
                    codeunit: r.id,
                    codeunit_name: o.name.clone(),
                    test: test.to_string(),
                    outcome: outcome.clone(),
                    call_stack,
                    assumptions,
                    time,
                    coverage,
                    messages,
                    captured,
                })
            };
            match r.result {
                Ok((done, stacks, times, hits, mut covs, mut scratch)) => {
                    for (name, outcome) in done {
                        let stack = stacks.iter().find(|(n, _)| *n == name).map(|(_, s)| s.clone());
                        let marks = hits.iter().find(|(n, _)| *n == name).map(|(_, h)| h.clone());
                        let marks = marks.unwrap_or_default();
                        if !marks.is_empty() {
                            marks_by_test.insert((r.si, r.id, name.to_lowercase()), marks.clone());
                        }
                        let time = times
                            .iter()
                            .find(|(n, _)| *n == name)
                            .map_or(std::time::Duration::ZERO, |(_, d)| *d);
                        let cov = covs
                            .iter()
                            .position(|(n, _)| *n == name)
                            .map(|i| std::mem::take(&mut covs[i].1))
                            .unwrap_or_default();
                        let said = scratch
                            .messages
                            .iter()
                            .position(|(n, _)| *n == name)
                            .map(|i| std::mem::take(&mut scratch.messages[i].1))
                            .unwrap_or_default();
                        let seen = scratch
                            .captured
                            .iter()
                            .position(|(n, _)| *n == name)
                            .map(|i| std::mem::take(&mut scratch.captured[i].1));
                        push(
                            &mut list,
                            &name,
                            &outcome,
                            stack.unwrap_or_default(),
                            marks,
                            time,
                            cov,
                            said,
                            seen,
                        );
                        times_by_case.insert((r.si, r.id, name.to_lowercase()), time);
                        outcomes.insert((r.si, r.id, name.to_lowercase()), outcome);
                    }
                }
                Err(msg) => {
                    panics += 1;
                    let panicked: Vec<&al_ir::Routine> = if opts.execute {
                        lowered.program.objects[r.oi]
                            .routines
                            .iter()
                            .filter(|t| t.is_trigger && t.name.eq_ignore_ascii_case("OnRun"))
                            .collect()
                    } else {
                        test_routines(&lowered.program.objects[r.oi]).collect()
                    };
                    for t in panicked {
                        let outcome = Outcome::NotRun(format!("runtime panic (bug): {msg}"));
                        push(
                            &mut list,
                            &t.name,
                            &outcome,
                            String::new(),
                            Vec::new(),
                            std::time::Duration::ZERO,
                            Vec::new(),
                            Vec::new(),
                            None,
                        );
                        outcomes.insert((r.si, r.id, t.name.to_lowercase()), outcome);
                    }
                }
            }
        }
        let tests_time = tests_started.elapsed();
        // Test codeunits of different runs may overlap in time: a run's time is what its codeunits took.
        let suite_times: Vec<(String, std::time::Duration)> = suites
            .iter()
            .enumerate()
            .map(|(si, suite)| {
                (
                    suite.name.clone(),
                    suite_work.iter().filter(|(i, _)| *i == si).map(|(_, d)| *d).sum(),
                )
            })
            .collect();
        codeunit_times.sort_by_key(|c| std::cmp::Reverse(c.took));
        codeunit_times.truncate(SLOWEST);
        let suites = suites.iter().map(|s| s.name.clone()).collect();
        // The workspace and the lowered program are big: dropping them takes as long as lowering
        // a tenth of the corpus, and nothing waits for it.
        let compile_parts = lowered.timings;
        let coverage_statements = if opts.coverage {
            crate::coverage::statements(&lowered.program, |oi| !lowered.dependency[oi], |a| order[a])
        } else {
            Vec::new()
        };
        // The engines hold every compiled routine and the baseline's tables: freeing them on this
        // thread kept the report waiting after the last test.
        retire((engine, baseline_engine, spare_engine));
        CorpusRuntime {
            outcomes,
            times: times_by_case,
            suites,
            panics,
            phases: Phases {
                compile: compile_time,
                baseline: baseline_time,
                tests: tests_time,
                suites: suite_times,
                compile_parts,
                cache: cache_report.clone(),
                engine: engine_time,
                baseline_skipped,
                baseline_why: baseline_note,
                compile_cache_skipped,
                source_index,
                slowest: codeunit_times,
            },
            details,
            results: list,
            dual_diffs,
            marks: marks_by_test,
            coverage_statements,
        }
    }
}

impl CorpusRuntime {
    /// With `RunOptions::dual_run`: the tests whose two runs differ, in the order of the codeunits.
    pub fn dual_diffs(&self) -> &[crate::dual::DualDiff] {
        &self.dual_diffs
    }

    /// Every test the run decided, with its codeunit's name and app, codeunit by codeunit.
    pub fn results(&self) -> &[TestResult] {
        &self.results
    }

    /// With `RunOptions::coverage`: every statement of the workspace's apps (the position ids that
    /// `TestResult::coverage` counts).
    pub fn coverage_statements(&self) -> &[crate::coverage::CoverageStatement] {
        &self.coverage_statements
    }

    /// Where `case` of the run named `suite` was refused or failed; only with `RunOptions::trace`.
    pub fn detail_in(&self, suite: &str, case: &CaseId) -> Option<&TestDetail> {
        let si = self.suites.iter().position(|s| s == suite)?;
        self.details.get(&(si, case.codeunit, case.name.to_lowercase()))
    }

    /// The unprobed assumptions `case` ran through in the run named `suite`; empty for a clean test.
    pub fn assumptions_in(&self, suite: &str, case: &CaseId) -> &[Assumption] {
        self.suites
            .iter()
            .position(|s| s == suite)
            .and_then(|si| self.marks.get(&(si, case.codeunit, case.name.to_lowercase())))
            .map_or(&[], Vec::as_slice)
    }

    /// How long `case` took in the first run.
    pub fn first_time(&self, case: &CaseId) -> std::time::Duration {
        self.times
            .get(&(0, case.codeunit, case.name.to_lowercase()))
            .copied()
            .unwrap_or_default()
    }

    /// How long `case` took in suite `suite` (zero for a test that was not started).
    pub fn time_in(&self, suite: &str, case: &CaseId) -> std::time::Duration {
        self.suites
            .iter()
            .position(|s| s == suite)
            .and_then(|si| self.times.get(&(si, case.codeunit, case.name.to_lowercase())))
            .copied()
            .unwrap_or_default()
    }

    /// The outcome of `case` in the run named `suite` (the first run has the empty name).
    pub fn outcome_in(&self, suite: &str, case: &CaseId) -> Outcome {
        self.suites
            .iter()
            .position(|s| s == suite)
            .and_then(|si| self.outcomes.get(&(si, case.codeunit, case.name.to_lowercase())))
            .cloned()
            .unwrap_or_else(|| Outcome::NotRun("test procedure not found in the compiled corpus".into()))
    }
}

impl CorpusRuntime {
    /// The outcome of `case` in the first run (the main corpus app, or the only run).
    pub fn first_outcome(&self, case: &CaseId) -> Outcome {
        self.outcomes
            .get(&(0, case.codeunit, case.name.to_lowercase()))
            .cloned()
            .unwrap_or_else(|| Outcome::NotRun("test procedure not found in the compiled corpus".into()))
    }
}

/// The key under which the install baseline of this run is kept on disk, or `None` when it must run every time.
/// The baseline is what the install code of the dependency apps and `Company-Initialize` write, followed by the
/// workspace's own install codeunits. It is the same for the same program and the same world around it, which is
/// everything the key holds: this runtime build, the dependency layers (the key of the top layer holds every app of
/// them and the target), the source of every other app, the test codeunits that were compiled, the BC target, the
/// environment of the host and the day. The day is there because the rows carry the time the run began (`SystemClock`),
/// and a test may ask how old one is.
///
/// Not kept at all: a run that reads test data, runs .NET, lets `File` use a real directory, reads the real clock as
/// it goes, records, compares two executable forms or has a test runner of its own: what such a run does is not in
/// the key.
#[allow(clippy::too_many_arguments)]
fn baseline_key(
    ws: &al_sema::Workspace,
    lowered: &Lowered,
    report: Option<&crate::compile_cache::CacheReport>,
    suites: &[SuiteRun],
    only: Option<&[(usize, u32)]>,
    target: Option<&al_version::Target>,
    dotnet: &al_vm::dotnet::DotNetConfig,
    opts: &RunOptions,
) -> Option<String> {
    use al_host::Clock;
    let _ = lowered;
    let report = report?;
    opts.compile_cache.as_ref()?;
    if opts.test_data.is_some()
        || opts.trace
        || opts.dual_run
        || opts.execute
        || opts.coverage
        || suites.iter().any(|s| s.runner.is_some())
        || dotnet.host.availability().is_ok()
        || crate::host::uses_real_files()
        || crate::host::real_clock()
    {
        return None;
    }
    let mut key = crate::baseline_cache::KeyBuilder::new();
    key.bytes("runtime_build", &crate::compile_cache::runtime_build());
    key.text("dependencies", &report.file.file_name()?.to_string_lossy());
    for (i, app) in ws.apps.iter().enumerate() {
        if ws.kinds.get(i) != Some(&al_sema::AppKind::Dependency) {
            key.text("app", &app.id);
            key.bytes("source", &crate::compile_cache::source_hash(app));
        }
    }
    let only: Vec<String> = only.unwrap_or(&[]).iter().map(|(a, id)| format!("{a}/{id}")).collect();
    key.text("only", &only.join(","));
    key.text("only_set", if opts.only.is_some() { "yes" } else { "no" });
    match target {
        Some(t) => key.text("target", &format!("{}/{}", t.bc, t.runtime)),
        None => key.text("target", "none"),
    };
    key.text("environment", crate::host::environment_text());
    let day = crate::host::SystemClock.now_utc_millis().div_euclid(86_400_000);
    key.text("day", &day.to_string());
    Some(key.finish())
}
