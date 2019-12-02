#![forbid(unsafe_code)]

#[macro_use]
extern crate log;

mod ast_walker;
mod deps;

use crate::ast_walker::IncludeTests;

use std::path::PathBuf;

use cargo::{
    core::{compiler::CompileMode, resolver::Method, shell::Shell},
    ops::CompileOptions,
    CliResult,
};
use structopt::StructOpt;

#[derive(StructOpt)]
pub struct Args {
    #[structopt(short = "o", value_name = "OUTPUT_FILE_PATH", parse(from_os_str))]
    /// Path to output file
    pub out_path: PathBuf,

    #[structopt(long = "package", short = "p", value_name = "SPEC")]
    /// Package to be used as the root of the tree
    pub package: Option<String>,

    #[structopt(long = "features", value_name = "FEATURES")]
    /// Space-separated list of features to activate
    pub features: Option<String>,

    #[structopt(long = "all-features")]
    /// Activate all available features
    pub all_features: bool,

    #[structopt(long = "no-default-features")]
    /// Do not activate the `default` feature
    pub no_default_features: bool,

    #[structopt(long = "target", value_name = "TARGET")]
    /// Set the target triple
    pub target: Option<String>,

    #[structopt(long = "all-targets")]
    /// Return dependencies for all targets. By default only the host target is matched.
    pub all_targets: bool,

    #[structopt(
        long = "manifest-path",
        value_name = "MANIFEST_PATH",
        parse(from_os_str)
    )]
    /// Path to Cargo.toml
    pub manifest_path: Option<PathBuf>,

    #[structopt(long = "invert", short = "i")]
    /// Invert the tree direction
    pub invert: bool,

    #[structopt(long = "jobs", short = "j")]
    /// Number of parallel jobs, defaults to # of CPUs
    pub jobs: Option<u32>,

    #[structopt(long = "verbose", short = "v", parse(from_occurrences))]
    /// Use verbose cargo output (-vv very verbose)
    pub verbose: u32,

    #[structopt(long = "quiet", short = "q")]
    /// Omit cargo output to stdout
    pub quiet: bool,

    #[structopt(long = "color", value_name = "WHEN")]
    /// Cargo output coloring: auto, always, never
    pub color: Option<String>,

    #[structopt(long = "frozen")]
    /// Require Cargo.lock and cache are up to date
    pub frozen: bool,

    #[structopt(long = "locked")]
    /// Require Cargo.lock is up to date
    pub locked: bool,

    #[structopt(short = "Z", value_name = "FLAG")]
    /// Unstable (nightly-only) flags to Cargo
    pub unstable_flags: Vec<String>,

    #[structopt(long = "include-tests")]
    /// Count unsafe usage in tests.
    pub include_tests: bool,

    #[structopt(long = "build-dependencies", alias = "build-deps")]
    /// Also analyze build dependencies
    pub build_deps: bool,

    #[structopt(long = "dev-dependencies", alias = "dev-deps")]
    /// Also analyze dev dependencies
    pub dev_deps: bool,

    #[structopt(long = "all-dependencies", alias = "all-deps")]
    /// Analyze all dependencies, including build and dev
    pub all_deps: bool,
}

/// Based on code from cargo-bloat. It seems weird that CompileOptions can be
/// constructed without providing all standard cargo options, TODO: Open an issue
/// in cargo?
pub fn build_compile_options<'a>(args: &'a Args, config: &'a cargo::Config) -> CompileOptions<'a> {
    let features = Method::split_features(&args.features.clone().into_iter().collect::<Vec<_>>())
        .into_iter()
        .map(|s| s.to_string());
    let mut opt = CompileOptions::new(&config, CompileMode::Check { test: false }).unwrap();
    opt.features = features.collect::<_>();
    opt.all_features = args.all_features;
    opt.no_default_features = args.no_default_features;

    // BuildConfig, see https://docs.rs/cargo/0.31.0/cargo/core/compiler/struct.BuildConfig.html
    if let Some(jobs) = args.jobs {
        opt.build_config.jobs = jobs;
    }

    opt
}

fn real_main(args: &Args, config: &mut cargo::Config) -> CliResult {
    let target_dir = None;
    config.configure(
        args.verbose,
        Some(args.quiet),
        &args.color,
        args.frozen,
        args.locked,
        &target_dir,
        &args.unstable_flags,
    )?;

    let ws = crate::deps::workspace(config, args.manifest_path.clone())?;
    let (packages, _) = cargo::ops::resolve_ws(&ws)?;

    info!("rustc config == {:?}", config.rustc(Some(&ws)));

    let copt = build_compile_options(args, config);
    let rs_files_used_in_compilation = crate::deps::resolve_rs_file_deps(&copt, &ws).unwrap();

    let allow_partial_results = true;
    let include_tests = if args.include_tests {
        IncludeTests::Yes
    } else {
        IncludeTests::No
    };
    let mut out_file =
        std::fs::File::create(&args.out_path).expect("Could not open output file for writing");

    let rs_files_scanned = crate::deps::find_unsafe_in_packages(
        &mut out_file,
        &packages,
        rs_files_used_in_compilation,
        allow_partial_results,
        include_tests,
    );

    rs_files_scanned
        .iter()
        .filter(|(_k, v)| **v == 0)
        .for_each(|(k, _v)| {
            // TODO: Ivestigate if this is related to code generated by build
            // scripts and/or macros. Some of the warnings of this kind is
            // printed for files somewhere under the "target" directory.
            // TODO: Find out if we can lookup PackageId associated with each
            // `.rs` file used by the build, including the file paths extracted
            // from `.d` dep files.
            warn!("Dependency file was never scanned: {}", k.display())
        });

    Ok(())
}

fn main() {
    env_logger::init();
    let mut config = match cargo::Config::default() {
        Ok(cfg) => cfg,
        Err(e) => {
            let mut shell = Shell::new();
            cargo::exit_with_error(e.into(), &mut shell)
        }
    };
    let args = Args::from_args();
    if let Err(e) = real_main(&args, &mut config) {
        let mut shell = Shell::new();
        cargo::exit_with_error(e, &mut shell)
    }
}
