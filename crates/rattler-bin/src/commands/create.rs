use std::{
    collections::HashMap,
    env,
    path::PathBuf,
    str::FromStr,
    time::{Duration, Instant},
};

use clap::ValueEnum;
use itertools::Itertools;
use miette::{Context, IntoDiagnostic};
use rattler::{
    default_cache_dir,
    install::{IndicatifReporter, Installer, Transaction, TransactionOperation},
    package_cache::PackageCache,
};
use rattler_conda_types::{
    Channel, ChannelConfig, GenericVirtualPackage, MatchSpec, Matches, NamelessMatchSpec,
    PackageName, ParseMatchSpecOptions, Platform, PrefixRecord, RepoDataRecord, Version,
};
use rattler_repodata_gateway::{Gateway, RepoData, SourceConfig};
use rattler_solve::{
    RepoDataIter, SolverImpl, SolverTask,
    libsolv_c::{self},
    resolvo::{
        self, EnvironmentCondition, EnvironmentLiteral, EnvironmentLiteralKind,
        SymbolicVirtualPackage, UniversalSolveError, UniversalSolverTask, display_condition,
        solve_universal,
    },
};

use crate::{
    commands::progress::{wrap_in_async_progress, wrap_in_progress},
    exclude_newer::ExcludeNewer,
    global_multi_progress,
};

/// Create a conda environment from package listing
///
/// Resolves and installs the specified packages into a target prefix,
/// pulling from the configured channels.
#[derive(Debug, clap::Parser)]
pub struct Opt {
    /// Channel to search for packages
    ///
    /// Example: -c conda-forge -c main
    #[clap(short, long = "channel")]
    channels: Option<Vec<String>>,

    /// Package specs to install
    #[clap(required = true)]
    specs: Vec<String>,

    /// Simulute command without installation
    #[clap(long)]
    dry_run: bool,

    /// Target platform (e.g., linux-64, osx-arm64)
    #[clap(long)]
    platform: Option<String>,

    #[clap(long)]
    virtual_package: Option<Vec<String>>,

    /// SAT Solver backend to use
    #[clap(long)]
    solver: Option<Solver>,

    /// Request solver timeout in milliseconds
    #[clap(long)]
    timeout: Option<u64>,

    /// Target prefix (environment path) for package installation
    #[clap(
        short = 'p',
        long = "prefix",
        visible_alias = "target-prefix",
        default_value = ".prefix"
    )]
    target_prefix: PathBuf,

    #[clap(long)]
    strategy: Option<SolveStrategy>,

    /// Only install dependencies of package specs
    #[clap(long, group = "deps_mode")]
    only_deps: bool,

    /// Only install package specifications without dependencies
    #[clap(long, group = "deps_mode")]
    no_deps: bool,

    /// Exclude packages that have been published after the specified timestamp.
    /// Can be specified as a timestamp (e.g., "2006-12-02T02:07:43Z") or as a date (e.g., "2006-12-02").
    /// When using a date, packages from the entire day are included.
    #[clap(long)]
    exclude_newer: Option<ExcludeNewer>,

    /// Run a universal solve (print-only) instead of a concrete install.
    ///
    /// The universal solver partitions the environment space defined by the
    /// symbolic virtual packages into cells and reports how the chosen
    /// records differ across environments (e.g. with/without CUDA, different
    /// glibc floors). No packages are installed; output is informational.
    #[clap(long)]
    universal: bool,

    /// Minimum CUDA version for the universal environment model.
    ///
    /// Only meaningful with --universal. Machines with a CUDA driver older
    /// than this version are outside the modeled space; the solve only
    /// covers "no CUDA" or "CUDA >= <floor>". Must be a version string such
    /// as "12.0".
    #[clap(long, default_value = "12.0")]
    cuda_floor: String,

    /// Minimum glibc version for the universal environment model (Linux only).
    ///
    /// Only meaningful with --universal on linux targets. Sets the lower bound
    /// of the glibc version range modeled. The upper bound is always <3.0.a0
    /// (ecosystem convention). Must be a version string such as "2.17".
    #[clap(long, default_value = "2.17")]
    glibc_floor: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SolveStrategy {
    /// Resolve the highest compatible version for every package.
    Highest,

    /// Resolve the lowest compatible version for every package.
    Lowest,

    /// Resolve the lowest compatible version for direct dependencies but the
    /// highest compatible for transitive dependencies.
    LowestDirect,
}

#[derive(Default, Debug, Clone, Copy, ValueEnum)]
pub enum Solver {
    #[default]
    Resolvo,
    #[value(name = "libsolv")]
    LibSolv,
}

impl From<SolveStrategy> for rattler_solve::SolveStrategy {
    fn from(value: SolveStrategy) -> Self {
        match value {
            SolveStrategy::Highest => rattler_solve::SolveStrategy::Highest,
            SolveStrategy::Lowest => rattler_solve::SolveStrategy::LowestVersion,
            SolveStrategy::LowestDirect => rattler_solve::SolveStrategy::LowestVersionDirect,
        }
    }
}

pub async fn create(opt: Opt) -> miette::Result<()> {
    let channel_config =
        ChannelConfig::default_with_root_dir(env::current_dir().into_diagnostic()?);
    // Make the target prefix absolute
    let target_prefix = std::path::absolute(opt.target_prefix).into_diagnostic()?;

    // Determine the platform we're going to install for
    let install_platform = if let Some(platform) = opt.platform {
        Platform::from_str(&platform).into_diagnostic()?
    } else {
        Platform::current()
    };

    println!("Installing for platform: {install_platform:?}");

    // Parse the specs from the command line. We do this explicitly instead of allow
    // clap to deal with this because we need to parse the `channel_config` when
    // parsing matchspecs.
    let match_spec_options = ParseMatchSpecOptions::strict()
        .with_extras(true)
        .with_conditionals(true)
        .with_flags(true);

    let specs = opt
        .specs
        .iter()
        .map(|spec| MatchSpec::from_str(spec, match_spec_options))
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()?;

    // Find the default cache directory. Create it if it doesn't exist yet.
    let cache_dir = default_cache_dir()
        .map_err(|e| miette::miette!("could not determine default cache directory: {}", e))?;
    rattler_cache::ensure_cache_dir(&cache_dir)
        .map_err(|e| miette::miette!("could not create cache directory: {}", e))?;

    // Determine the channels to use from the command line or select the default.
    // Like matchspecs this also requires the use of the `channel_config` so we
    // have to do this manually.
    let channels = opt
        .channels
        .unwrap_or_else(|| vec![String::from("conda-forge")])
        .into_iter()
        .map(|channel_str| Channel::from_str(channel_str, &channel_config))
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()?;

    // Determine the packages that are currently installed in the environment.
    let installed_packages =
        PrefixRecord::collect_from_prefix::<PrefixRecord>(&target_prefix).into_diagnostic()?;

    // For each channel/subdirectory combination, download and cache the
    // `repodata.json` that should be available from the corresponding Url. The
    // code below also displays a nice CLI progress-bar to give users some more
    // information about what is going on.
    let download_client = super::client::create_client_with_middleware()?;

    // Get the package names from the matchspecs so we can only load the package
    // records that we need.
    let gateway = Gateway::builder()
        .with_cache_dir(cache_dir.join(rattler_cache::REPODATA_CACHE_DIR))
        .with_package_cache(PackageCache::new(
            cache_dir.join(rattler_cache::PACKAGE_CACHE_DIR),
        ))
        .with_client(download_client.clone())
        .with_channel_config(rattler_repodata_gateway::ChannelConfig {
            default: SourceConfig {
                sharded_enabled: true,
                ..SourceConfig::default()
            },
            per_channel: HashMap::new(),
        })
        .finish();

    let start_load_repo_data = Instant::now();
    let repo_data = wrap_in_async_progress(
        "loading repodata",
        gateway
            .query(
                channels,
                [install_platform, Platform::NoArch],
                specs.clone(),
            )
            .recursive(true),
    )
    .await
    .into_diagnostic()
    .context("failed to load repodata")?;

    // Determine the number of records
    let total_records: usize = repo_data.iter().map(RepoData::len).sum();
    println!(
        "Loaded {} records in {:?}",
        total_records,
        start_load_repo_data.elapsed()
    );

    // Determine virtual packages of the system. These packages define the
    // capabilities of the system. Some packages depend on these virtual
    // packages to indicate compatibility with the hardware of the system.
    let virtual_packages = wrap_in_progress("determining virtual packages", move || {
        if let Some(virtual_packages) = opt.virtual_package {
            Ok(virtual_packages
                .iter()
                .map(|virt_pkg| {
                    let elems = virt_pkg.split('=').collect::<Vec<&str>>();
                    Ok(GenericVirtualPackage {
                        name: elems[0].try_into().into_diagnostic()?,
                        version: elems
                            .get(1)
                            .map_or(Version::from_str("0"), |s| Version::from_str(s))
                            .into_diagnostic()?,
                        build_string: (*elems.get(2).unwrap_or(&"")).to_string(),
                    })
                })
                .collect::<miette::Result<Vec<_>>>()?)
        } else {
            rattler_virtual_packages::VirtualPackage::detect(
                &rattler_virtual_packages::VirtualPackageOverrides::from_env(),
            )
            .map(|vpkgs| {
                vpkgs
                    .iter()
                    .map(|vpkg| GenericVirtualPackage::from(vpkg.clone()))
                    .collect::<Vec<_>>()
            })
            .into_diagnostic()
        }
    })?;

    println!(
        "Virtual packages:\n{}\n",
        virtual_packages
            .iter()
            .format_with("\n", |i, f| f(&format_args!("  - {i}",)))
    );

    if opt.universal {
        return run_universal_solve(
            install_platform,
            repo_data,
            specs,
            virtual_packages,
            opt.cuda_floor,
            opt.glibc_floor,
            opt.timeout,
            opt.strategy,
            opt.exclude_newer,
        );
    }

    // Now that we parsed and downloaded all information, construct the packaging
    // problem that we need to solve. We do this by constructing a
    // `SolverProblem`. This encapsulates all the information required to be
    // able to solve the problem.
    let locked_packages: Vec<&RepoDataRecord> = installed_packages
        .iter()
        .map(|record| &record.repodata_record)
        .collect();

    let solver_task = SolverTask {
        locked_packages,
        virtual_packages,
        specs: specs.clone(),
        timeout: opt.timeout.map(Duration::from_millis),
        strategy: opt.strategy.map_or_else(Default::default, Into::into),
        exclude_newer: opt.exclude_newer.map(Into::into),
        ..SolverTask::from_iter(&repo_data)
    };

    // Next, use a solver to solve this specific problem. This provides us with all
    // the operations we need to apply to our environment to bring it up to
    // date.
    let solver_result = wrap_in_progress("solving", move || match opt.solver.unwrap_or_default() {
        Solver::Resolvo => resolvo::Solver.solve(solver_task),
        Solver::LibSolv => libsolv_c::Solver.solve(solver_task),
    })
    .into_diagnostic()?;

    let mut required_packages: Vec<RepoDataRecord> = solver_result.records;

    if opt.no_deps {
        required_packages.retain(|r| specs.iter().any(|s| s.matches(&r.package_record)));
    } else if opt.only_deps {
        required_packages.retain(|r| !specs.iter().any(|s| s.matches(&r.package_record)));
    };

    if opt.dry_run {
        // Construct a transaction to
        let transaction = Transaction::from_current_and_desired(
            installed_packages,
            required_packages,
            None,
            None, // ignored packages
            install_platform,
        )
        .into_diagnostic()?;

        if transaction.operations.is_empty() {
            println!("No operations necessary");
        } else {
            print_transaction(&transaction, solver_result.extras);
        }

        return Ok(());
    }

    let install_start = Instant::now();
    let result = Installer::new()
        .with_download_client(download_client)
        .with_target_platform(install_platform)
        .with_installed_packages(installed_packages)
        .with_execute_link_scripts(true)
        .with_requested_specs(specs)
        .with_reporter(
            IndicatifReporter::builder()
                .with_multi_progress(global_multi_progress())
                .finish(),
        )
        .install(&target_prefix, required_packages)
        .await
        .into_diagnostic()?;

    if result.transaction.operations.is_empty() {
        println!(
            "{} Already up to date",
            console::style(console::Emoji("✔", "")).green(),
        );
    } else {
        println!(
            "{} Successfully updated the environment in {:?}",
            console::style(console::Emoji("✔", "")).green(),
            install_start.elapsed()
        );
        // Since operations are nonempty we can safely unwrap.
        let transaction = result
            .transaction
            .into_prefix_record(target_prefix)
            .unwrap();
        print_transaction(&transaction, solver_result.extras);
    }

    Ok(())
}

/// Runs the universal solve path and prints a human-readable report.
///
/// This is a separate (synchronous) function so it can return early without
/// holding async resources. No packages are installed; the output is purely
/// informational and intended for manual testing of the universal solver.
#[allow(clippy::too_many_arguments)]
fn run_universal_solve(
    install_platform: Platform,
    repo_data: Vec<RepoData>,
    specs: Vec<MatchSpec>,
    concrete_virtual_packages: Vec<GenericVirtualPackage>,
    cuda_floor: String,
    glibc_floor: String,
    timeout_ms: Option<u64>,
    strategy: Option<SolveStrategy>,
    exclude_newer: Option<ExcludeNewer>,
) -> miette::Result<()> {
    // Names of virtual packages that are treated symbolically in universal
    // mode. These are excluded from the concrete virtual_packages list.
    let symbolic_names: &[&str] = &["__cuda", "__glibc", "__osx", "__win"];

    // Build the symbolic virtual package set for the target platform.
    // Only include packages relevant to the platform being modeled.
    let mut symbolic_virtual_packages: Vec<SymbolicVirtualPackage> = Vec::new();

    // __cuda is absentable on all platforms (machines without a GPU driver).
    symbolic_virtual_packages.push(SymbolicVirtualPackage {
        name: PackageName::new_unchecked("__cuda"),
        can_be_absent: true,
    });

    // Platform-specific non-absentable virtual packages.
    if install_platform.is_linux() {
        symbolic_virtual_packages.push(SymbolicVirtualPackage {
            name: PackageName::new_unchecked("__glibc"),
            can_be_absent: false,
        });
    } else if install_platform.is_osx() {
        symbolic_virtual_packages.push(SymbolicVirtualPackage {
            name: PackageName::new_unchecked("__osx"),
            can_be_absent: false,
        });
    } else if install_platform.is_windows() {
        symbolic_virtual_packages.push(SymbolicVirtualPackage {
            name: PackageName::new_unchecked("__win"),
            can_be_absent: false,
        });
    }

    // Filter out the symbolic names from the concrete virtual packages, so we
    // do not inject them as concrete records. Other detected virtual packages
    // (e.g. __unix, __linux, __archspec) stay concrete as usual.
    let concrete_vps: Vec<GenericVirtualPackage> = concrete_virtual_packages
        .into_iter()
        .filter(|vp| {
            !symbolic_names
                .iter()
                .any(|name| vp.name.as_normalized() == *name)
        })
        .collect();

    // Build the environment model: a CNF bounding the environment space.
    //
    // Rules (see design doc section 3 and test scenario c2 for the bound
    // rationale):
    //   - Linux: __glibc >=<floor>,<3.0.a0  (the <3.0.a0 upper bound is the
    //     ecosystem-exact bound used in conda-forge deps; omitting it causes
    //     vacuous unsolvable regions for glibc >= 3.0a0).
    //   - All platforms: __cuda absent OR __cuda >=<floor>  (modeled as a
    //     single disjunction clause).
    //   - osx/win: __osx/__win floor literal (minimal; a single clause).
    let cuda_floor_version =
        Version::from_str(&cuda_floor).map_err(|e| miette::miette!("invalid --cuda-floor: {e}"))?;
    let glibc_floor_version = Version::from_str(&glibc_floor)
        .map_err(|e| miette::miette!("invalid --glibc-floor: {e}"))?;

    let mut environment_model: Vec<EnvironmentCondition> = Vec::new();

    // CUDA clause: absent OR >= floor
    {
        let cuda_name = PackageName::new_unchecked("__cuda");
        let absent_lit = EnvironmentLiteral {
            package: cuda_name.clone(),
            kind: EnvironmentLiteralKind::Absent,
        };
        let matches_lit = EnvironmentLiteral {
            package: cuda_name,
            kind: EnvironmentLiteralKind::Matches(
                NamelessMatchSpec::from_str(
                    &format!(">={cuda_floor_version}"),
                    rattler_conda_types::ParseStrictness::Strict,
                )
                .map_err(|e| miette::miette!("failed to build cuda model literal: {e}"))?,
            ),
        };
        // disjunction: (absent=true) OR (matches=true)
        environment_model.push(vec![(absent_lit, true), (matches_lit, true)]);
    }

    // Platform-specific floor clause.
    if install_platform.is_linux() {
        let glibc_name = PackageName::new_unchecked("__glibc");
        let glibc_lit = EnvironmentLiteral {
            package: glibc_name,
            kind: EnvironmentLiteralKind::Matches(
                NamelessMatchSpec::from_str(
                    &format!(">={glibc_floor_version},<3.0.a0"),
                    rattler_conda_types::ParseStrictness::Strict,
                )
                .map_err(|e| miette::miette!("failed to build glibc model literal: {e}"))?,
            ),
        };
        // single-literal clause: glibc must be in [floor, 3.0a0)
        environment_model.push(vec![(glibc_lit, true)]);
    } else if install_platform.is_osx() {
        // A minimal osx floor: >=11.0 covers the modern range; keep it loose
        // so a wide range of macOS versions are in the modeled space.
        let osx_name = PackageName::new_unchecked("__osx");
        let osx_lit = EnvironmentLiteral {
            package: osx_name,
            kind: EnvironmentLiteralKind::Matches(
                NamelessMatchSpec::from_str(">=11.0", rattler_conda_types::ParseStrictness::Strict)
                    .map_err(|e| miette::miette!("failed to build osx model literal: {e}"))?,
            ),
        };
        environment_model.push(vec![(osx_lit, true)]);
    } else if install_platform.is_windows() {
        // A minimal win floor: >=10 covers modern Windows.
        let win_name = PackageName::new_unchecked("__win");
        let win_lit = EnvironmentLiteral {
            package: win_name,
            kind: EnvironmentLiteralKind::Matches(
                NamelessMatchSpec::from_str(">=10", rattler_conda_types::ParseStrictness::Strict)
                    .map_err(|e| miette::miette!("failed to build win model literal: {e}"))?,
            ),
        };
        environment_model.push(vec![(win_lit, true)]);
    }

    // Use from_env() overrides in the universal path, matching the intent of
    // the user's pending change to the concrete path. This is a SEPARATE call
    // site: the concrete path's call site above is intentionally left intact.
    let machine_virtual_packages = rattler_virtual_packages::VirtualPackage::detect(
        &rattler_virtual_packages::VirtualPackageOverrides::from_env(),
    )
    .map(|vpkgs| {
        vpkgs
            .iter()
            .map(|vpkg| GenericVirtualPackage::from(vpkg.clone()))
            .collect::<Vec<_>>()
    })
    .into_diagnostic()
    .context("failed to detect virtual packages for projection")?;

    let task = UniversalSolverTask {
        available_packages: repo_data
            .iter()
            .map(|rd| RepoDataIter(rd.iter()))
            .collect::<Vec<_>>(),
        virtual_packages: concrete_vps,
        specs: specs.clone(),
        constraints: Vec::new(),
        timeout: timeout_ms.map(Duration::from_millis),
        cancellation_token: None,
        channel_priority: rattler_solve::ChannelPriority::default(),
        exclude_newer: exclude_newer.map(Into::into),
        strategy: strategy.map_or_else(Default::default, Into::into),
        symbolic_virtual_packages,
        environment_model,
        seed_partition: Vec::new(),
    };

    println!(
        "\n{} Running universal solve...\n",
        console::style("*").cyan().bold()
    );

    let solution = match wrap_in_progress("solving (universal)", move || solve_universal(task)) {
        Ok(sol) => sol,
        Err(UniversalSolveError::Unsolvable {
            condition,
            conflict,
            condition_literals: _,
        }) => {
            println!(
                "{} Universal solve failed: no solution exists for environments where:\n  {}\n",
                console::style("!").red().bold(),
                console::style(&condition).yellow()
            );
            println!(
                "{} Conflict details:\n{}",
                console::style("!").red().bold(),
                conflict
            );
            println!(
                "\n{} This is expected when the environment model covers a version range that\n  \
                 no package in the channel supports. For example, a CUDA floor below the\n  \
                 minimum version required by available packages will fail here.",
                console::style("hint:").cyan()
            );
            return Ok(());
        }
        Err(UniversalSolveError::Cancelled) => {
            return Err(miette::miette!("universal solve was cancelled (timeout?)"));
        }
        Err(UniversalSolveError::Setup(e)) => {
            return Err(miette::miette!("universal solve setup error: {e}"));
        }
    };

    // Print verification status.
    match solution.verify() {
        Ok(()) => {
            println!(
                "{} Solution verification passed.\n",
                console::style("ok").green().bold()
            );
        }
        Err(violations) => {
            println!(
                "{} Solution verification found {} violation(s):",
                console::style("warning:").yellow().bold(),
                violations.len()
            );
            for v in violations {
                println!("  - {v}");
            }
            println!();
        }
    }

    // Print each cell.
    println!(
        "{} Universal solution: {} cell(s)\n",
        console::style("=>").green().bold(),
        solution.cells.len()
    );

    for (cell_idx, (condition, records)) in solution.cells.iter().enumerate() {
        let condition_str = display_condition(condition);
        println!(
            "  {} Cell {}: {}",
            console::style(format!("[{}/{}]", cell_idx + 1, solution.cells.len()))
                .cyan()
                .bold(),
            cell_idx + 1,
            console::style(&condition_str).yellow()
        );
        if records.is_empty() {
            println!("    (no records)");
        } else {
            for r in records {
                println!(
                    "    {} {} {} {}",
                    console::style("+").green(),
                    r.package_record.name.as_normalized(),
                    r.package_record.version,
                    r.package_record.build,
                );
            }
        }
        println!();
    }

    // Print divergence summary: packages whose record differs across cells.
    let diverging: Vec<_> = solution
        .merged
        .iter()
        .filter(|(_record, presence)| {
            // Diverging: presence is not "all environments" (i.e. not a single
            // empty-condition entry in the presence list).
            !(presence.len() == 1 && presence[0].is_empty())
        })
        .collect();

    if diverging.is_empty() {
        println!(
            "{} No divergence: all cells resolve to the same package set.\n",
            console::style("ok").green().bold()
        );
    } else {
        println!(
            "{} Diverging packages ({} total):\n",
            console::style("divergence:").yellow().bold(),
            diverging.len()
        );
        for (record, presence) in &diverging {
            let presence_str = presence
                .iter()
                .map(|c| format!("({})", display_condition(c)))
                .join(" OR ");
            println!(
                "  {} {}={}={} present when: {}",
                console::style("~").yellow(),
                record.package_record.name.as_normalized(),
                record.package_record.version,
                record.package_record.build,
                console::style(&presence_str).dim(),
            );
        }
        println!();
    }

    // Projection: which cell matches this machine?
    println!(
        "{} Projection for current machine:",
        console::style("machine:").cyan().bold()
    );
    println!(
        "  Detected virtual packages: {}",
        machine_virtual_packages
            .iter()
            .map(|vp| format!("{vp}"))
            .join(", ")
    );

    match solution.project(&machine_virtual_packages) {
        Some(cell_records) => {
            // Find which cell index this is for reporting.
            let cell_idx = solution
                .cells
                .iter()
                .position(|(_, r)| r.as_slice() == cell_records)
                .map_or(0, |i| i + 1);
            let condition_str = solution
                .cells
                .iter()
                .find(|(_, r)| r.as_slice() == cell_records)
                .map(|(c, _)| display_condition(c))
                .unwrap_or_default();
            println!(
                "  {} This machine matches cell {} ({})\n",
                console::style("=>").green().bold(),
                cell_idx,
                console::style(&condition_str).yellow()
            );
            for r in cell_records {
                println!(
                    "    {} {} {} {}",
                    console::style("+").green(),
                    r.package_record.name.as_normalized(),
                    r.package_record.version,
                    r.package_record.build,
                );
            }
        }
        None => {
            println!(
                "  {} This machine is outside the environment model.\n  \
                 (Try adjusting --cuda-floor or --glibc-floor to include this machine.)",
                console::style("!").red().bold(),
            );
        }
    }

    Ok(())
}

/// Prints the operations of the transaction to the console.
fn print_transaction(
    transaction: &Transaction<PrefixRecord, RepoDataRecord>,
    features: HashMap<PackageName, Vec<String>>,
) {
    let format_record = |r: &RepoDataRecord| {
        let direct_url_print = if let Some(channel) = &r.channel {
            channel.clone()
        } else {
            String::new()
        };

        if let Some(features) = features.get(&r.package_record.name) {
            format!(
                "{}[{}] {} {} {}",
                r.package_record.name.as_normalized(),
                features.join(", "),
                r.package_record.version,
                r.package_record.build,
                direct_url_print,
            )
        } else {
            format!(
                "{} {} {} {}",
                r.package_record.name.as_normalized(),
                r.package_record.version,
                r.package_record.build,
                direct_url_print,
            )
        }
    };

    for operation in &transaction.operations {
        match operation {
            TransactionOperation::Install(r) => {
                println!("{} {}", console::style("+").green(), format_record(r));
            }
            TransactionOperation::Change { old, new } => {
                println!(
                    "{} {} -> {}",
                    console::style("~").yellow(),
                    format_record(&old.repodata_record),
                    format_record(new)
                );
            }
            TransactionOperation::Reinstall { old, .. } => {
                println!(
                    "{} {}",
                    console::style("~").yellow(),
                    format_record(&old.repodata_record)
                );
            }
            TransactionOperation::Remove(r) => {
                println!(
                    "{} {}",
                    console::style("-").red(),
                    format_record(&r.repodata_record)
                );
            }
        }
    }
}
