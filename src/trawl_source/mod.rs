mod ast_walker;

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use cargo::{
    core::{
        compiler::{CompileMode, Executor, Unit},
        manifest::TargetKind,
        package::PackageSet,
        Package, PackageId, Target, Workspace,
    },
    ops::{CleanOptions, CompileOptions},
    util::{paths, CargoResult, ProcessBuilder},
    Config,
};

use cargo::CliResult;
use structopt::StructOpt;
use walkdir::{self, WalkDir};

#[derive(Debug)]
pub(crate) enum RsResolveError {
    Walkdir(walkdir::Error),

    /// Like io::Error but with the related path.
    Io(io::Error, PathBuf),

    /// Would like cargo::Error here, but it's private, why?
    /// This is still way better than a panic though.
    Cargo(String),

    /// This should not happen unless incorrect assumptions have been made in
    /// `siderophile` about how the cargo API works.
    ArcUnwrap(),

    /// Failed to get the inner context out of the mutex.
    InnerContextMutex(String),

    /// Failed to parse a .dep file.
    DepParse(String, PathBuf),
}

impl Error for RsResolveError {}

/// Forward Display to Debug.
impl fmt::Display for RsResolveError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl From<PoisonError<CustomExecutorInnerContext>> for RsResolveError {
    fn from(e: PoisonError<CustomExecutorInnerContext>) -> Self {
        RsResolveError::InnerContextMutex(e.to_string())
    }
}

fn is_file_with_ext(entry: &walkdir::DirEntry, file_ext: &str) -> bool {
    if !entry.file_type().is_file() {
        return false;
    }
    let p = entry.path();
    let ext = match p.extension() {
        Some(e) => e,
        None => return false,
    };
    // to_string_lossy is ok since we only want to match against an ASCII
    // compatible extension and we do not keep the possibly lossy result
    // around.
    ext.to_string_lossy() == file_ext
}

// TODO: Make a wrapper type for canonical paths and hide all mutable access.

/// Provides information needed to scan for crate root
/// `#![forbid(unsafe_code)]`.
/// The wrapped PathBufs are canonicalized.
enum RsFile {
    /// Library entry point source file, usually src/lib.rs
    LibRoot(PathBuf),

    /// Executable entry point source file, usually src/main.rs
    BinRoot(PathBuf),

    /// Not sure if this is relevant but let's be conservative for now.
    CustomBuildRoot(PathBuf),

    /// All other .rs files.
    Other(PathBuf),
}

impl RsFile {
    fn as_path_buf(&self) -> &PathBuf {
        match self {
            RsFile::LibRoot(ref pb) => pb,
            RsFile::BinRoot(ref pb) => pb,
            RsFile::CustomBuildRoot(ref pb) => pb,
            RsFile::Other(ref pb) => pb,
        }
    }
}

pub fn find_rs_files_in_dir(dir: &Path) -> impl Iterator<Item = PathBuf> {
    let walker = WalkDir::new(dir).into_iter();
    walker.filter_map(|entry| {
        let entry = entry.expect("walkdir error."); // TODO: Return result.
        if !is_file_with_ext(&entry, "rs") {
            return None;
        }
        Some(
            entry
                .path()
                .canonicalize()
                .expect("Error converting to canonical path"),
        ) // TODO: Return result.
    })
}

fn find_rs_files_in_package(pack: &Package) -> Vec<RsFile> {
    // Find all build target entry point source files.
    let mut canon_targets = HashMap::new();
    for t in pack.targets() {
        let path = match t.src_path().path() {
            Some(p) => p,
            None => continue,
        };
        if !path.exists() {
            // A package published to crates.io is not required to include
            // everything. We have to skip this build target.
            continue;
        }
        let canon = path
            .canonicalize() // will Err on non-existing paths.
            .expect("canonicalize for build target path failed."); // FIXME
        let targets = canon_targets.entry(canon).or_insert_with(Vec::new);
        targets.push(t);
    }
    let mut out = Vec::new();
    for p in find_rs_files_in_dir(pack.root()) {
        if !canon_targets.contains_key(&p) {
            out.push(RsFile::Other(p));
        }
    }
    for (k, v) in canon_targets.into_iter() {
        for target in v {
            out.push(into_rs_code_file(target.kind(), k.clone()));
        }
    }
    out
}

fn into_rs_code_file(kind: &TargetKind, path: PathBuf) -> RsFile {
    match kind {
        TargetKind::Lib(_) => RsFile::LibRoot(path),
        TargetKind::Bin => RsFile::BinRoot(path),
        TargetKind::Test => RsFile::Other(path),
        TargetKind::Bench => RsFile::Other(path),
        TargetKind::ExampleLib(_) => RsFile::Other(path),
        TargetKind::ExampleBin => RsFile::Other(path),
        TargetKind::CustomBuild => RsFile::CustomBuildRoot(path),
    }
}

fn find_rs_files_in_packages<'a>(
    packs: &'a Vec<&Package>,
) -> impl Iterator<Item = (PackageId, RsFile)> + 'a {
    packs.iter().flat_map(|pack| {
        find_rs_files_in_package(pack)
            .into_iter()
            .map(move |path| (pack.package_id(), path))
    })
}

/// This is mostly `PackageSet::get_many`. The only difference is that we don't panic when
/// downloads fail
fn get_many<'a>(
    packs: &'a PackageSet,
    ids: impl IntoIterator<Item = PackageId>,
) -> Vec<&'a Package> {
    let mut pkgs = Vec::new();
    let mut downloads = packs.enable_download().unwrap();
    for id in ids {
        match downloads.start(id) {
            // This might not return `Some` right away. It's still downloading.
            Ok(pkg_opt) => pkgs.extend(pkg_opt),
            Err(e) => warn!("Could not begin downloading {:?}, {:?}", id, e),
        }
    }
    while downloads.remaining() > 0 {
        // Packages whose `.start()` returned an `Ok(None)` earlier will return now
        match downloads.wait() {
            Ok(pkg) => pkgs.push(pkg),
            Err(e) => warn!("Failed to download package, {:?}", e),
        }
    }
    pkgs
}

/// Finds and outputs all unsafe things to the given file
pub(crate) fn find_unsafe_in_packages<'a, 'b>(
    out_file: &mut std::fs::File,
    packs: &'a PackageSet<'b>,
    mut rs_files_used: HashMap<PathBuf, u32>,
    allow_partial_results: bool,
    include_tests: ast_walker::IncludeTests,
) -> HashMap<PathBuf, u32> {
    let packs = get_many(packs, packs.package_ids());
    let pack_code_files = find_rs_files_in_packages(&packs);
    for (pack_id, rs_code_file) in pack_code_files {
        let p = rs_code_file.as_path_buf();

        // This .rs file path was found by intercepting rustc arguments or by parsing the .d files
        // produced by rustc. Here we increase the counter for this path to mark that this file has
        // been scanned. Warnings will be printed for .rs files in this collection with a count of
        // 0 (has not been scanned). If this happens, it could indicate a logic error or some
        // incorrect assumption in siderophile.
        rs_files_used.get_mut(p).map(|c| *c += 1);

        let crate_name = pack_id.name().as_str().replace("-", "_");
        match ast_walker::find_unsafe_in_file(&crate_name, p, include_tests) {
            Ok(ast_walker::UnsafeItems(items)) => {
                // Output unsafe items as we go
                for item in items {
                    writeln!(out_file, "{}", item).expect("Error writing to out file");
                }
            }
            Err(e) => match allow_partial_results {
                true => warn!(
                    "Failed to parse file: {}, {:?}. Continuing...",
                    p.display(),
                    e
                ),
                false => panic!("Failed to parse file: {}, {:?} ", p.display(), e),
            },
        }
    }

    rs_files_used
}

/// Trigger a `cargo clean` + `cargo check` and listen to the cargo/rustc
/// communication to figure out which source files were used by the build.
pub(crate) fn resolve_rs_file_deps(
    copt: &CompileOptions,
    ws: &Workspace,
) -> Result<HashMap<PathBuf, u32>, RsResolveError> {
    let config = ws.config();
    // Need to run a cargo clean to identify all new .d deps files.
    // TODO: Figure out how this can be avoided to improve performance, clean
    // Rust builds are __slow__.
    let clean_opt = CleanOptions {
        config: &config,
        spec: vec![],
        target: None,
        profile_specified: false,
        requested_profile: copt.build_config.requested_profile,
        doc: false,
    };
    cargo::ops::clean(ws, &clean_opt).map_err(|e| RsResolveError::Cargo(e.to_string()))?;
    let inner_arc = Arc::new(Mutex::new(CustomExecutorInnerContext::default()));
    {
        let cust_exec = CustomExecutor {
            cwd: config.cwd().to_path_buf(),
            inner_ctx: inner_arc.clone(),
        };
        let exec: Arc<dyn Executor> = Arc::new(cust_exec);
        cargo::ops::compile_with_exec(ws, &copt, &exec)
            .map_err(|e| RsResolveError::Cargo(e.to_string()))?;
    }
    let ws_root = ws.root().to_path_buf();
    let inner_mutex = Arc::try_unwrap(inner_arc).map_err(|_| RsResolveError::ArcUnwrap())?;
    let (rs_files, out_dir_args) = {
        let ctx = inner_mutex.into_inner()?;
        (ctx.rs_file_args, ctx.out_dir_args)
    };
    let mut hm = HashMap::<PathBuf, u32>::new();
    for out_dir in out_dir_args {
        // TODO: Figure out if the `.d` dep files are used by one or more rustc
        // calls. It could be useful to know which `.d` dep files belong to
        // which rustc call. That would allow associating each `.rs` file found
        // in each dep file with a PackageId.
        for ent in WalkDir::new(&out_dir) {
            let ent = ent.map_err(RsResolveError::Walkdir)?;
            if !is_file_with_ext(&ent, "d") {
                continue;
            }
            let deps = parse_rustc_dep_info(ent.path())
                .map_err(|e| RsResolveError::DepParse(e.to_string(), ent.path().to_path_buf()))?;
            let canon_paths = deps
                .into_iter()
                .flat_map(|t| t.1)
                .map(PathBuf::from)
                .map(|pb| ws_root.join(pb))
                .map(|pb| pb.canonicalize().map_err(|e| RsResolveError::Io(e, pb)));
            for p in canon_paths {
                hm.insert(p?, 0);
            }
        }
    }
    for pb in rs_files {
        // rs_files must already be canonicalized
        hm.insert(pb, 0);
    }
    Ok(hm)
}

/// Copy-pasted (almost) from the private module cargo::core::compiler::fingerprint.
///
/// TODO: Make a PR to the cargo project to expose this function or to expose
/// the dependency data in some other way.
fn parse_rustc_dep_info(rustc_dep_info: &Path) -> CargoResult<Vec<(String, Vec<String>)>> {
    let contents = paths::read(rustc_dep_info)?;
    contents
        .lines()
        .filter_map(|l| l.find(": ").map(|i| (l, i)))
        .map(|(line, pos)| {
            let target = &line[..pos];
            let mut deps = line[pos + 2..].split_whitespace();
            let mut ret = Vec::new();
            while let Some(s) = deps.next() {
                let mut file = s.to_string();
                while file.ends_with('\\') {
                    file.pop();
                    file.push(' ');
                    //file.push_str(deps.next().ok_or_else(|| {
                    //internal("malformed dep-info format, trailing \\".to_string())
                    //})?);
                    file.push_str(deps.next().expect("malformed dep-info format, trailing \\"));
                }
                ret.push(file);
            }
            Ok((target.to_string(), ret))
        })
        .collect()
}

#[derive(Debug, Default)]
struct CustomExecutorInnerContext {
    /// Stores all lib.rs, main.rs etc. passed to rustc during the build.
    rs_file_args: HashSet<PathBuf>,

    /// Investigate if this needs to be intercepted like this or if it can be
    /// looked up in a nicer way.
    out_dir_args: HashSet<PathBuf>,
}

use std::sync::PoisonError;

/// A cargo Executor to intercept all build tasks and store all ".rs" file
/// paths for later scanning.
///
/// TODO: This is the place(?) to make rustc perform macro expansion to allow
/// scanning of the the expanded code. (incl. code generated by build.rs).
/// Seems to require nightly rust.
#[derive(Debug)]
struct CustomExecutor {
    /// Current work dir
    cwd: PathBuf,

    /// Needed since multiple rustc calls can be in flight at the same time.
    inner_ctx: Arc<Mutex<CustomExecutorInnerContext>>,
}

use std::error::Error;
use std::fmt;

#[derive(Debug)]
enum CustomExecutorError {
    OutDirKeyMissing(String),
    OutDirValueMissing(String),
    InnerContextMutex(String),
    Io(io::Error, PathBuf),
}

impl Error for CustomExecutorError {}

/// Forward Display to Debug. See the crate root documentation.
impl fmt::Display for CustomExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl Executor for CustomExecutor {
    /// In case of an `Err`, Cargo will not continue with the build process for
    /// this package.
    /// TODO: add doing things with on_stdout_line and on_stderr_line
    fn exec(
        &self,
        cmd: ProcessBuilder,
        _id: PackageId,
        _target: &Target,
        _mode: CompileMode,
        _on_stdout_line: &mut dyn FnMut(&str) -> CargoResult<()>,
        _on_stderr_line: &mut dyn FnMut(&str) -> CargoResult<()>,
    ) -> CargoResult<()> {
        let args = cmd.get_args();
        let out_dir_key = OsString::from("--out-dir");
        let out_dir_key_idx = args
            .iter()
            .position(|s| *s == out_dir_key)
            .ok_or_else(|| CustomExecutorError::OutDirKeyMissing(cmd.to_string()))?;
        let out_dir = args
            .get(out_dir_key_idx + 1)
            .ok_or_else(|| CustomExecutorError::OutDirValueMissing(cmd.to_string()))
            .map(PathBuf::from)?;

        // This can be different from the cwd used to launch the wrapping cargo
        // plugin. Discovered while fixing
        // https://github.com/anderejd/cargo-geiger/issues/19
        let cwd = cmd
            .get_cwd()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd.to_owned());

        {
            // Scope to drop and release the mutex before calling rustc.
            let mut ctx = self
                .inner_ctx
                .lock()
                .map_err(|e| CustomExecutorError::InnerContextMutex(e.to_string()))?;
            for tuple in args
                .iter()
                .map(|s| (s, s.to_string_lossy().to_lowercase()))
                .filter(|t| t.1.ends_with(".rs"))
            {
                let raw_path = cwd.join(tuple.0);
                let p = raw_path
                    .canonicalize()
                    .map_err(|e| CustomExecutorError::Io(e, raw_path))?;
                ctx.rs_file_args.insert(p);
            }
            ctx.out_dir_args.insert(out_dir);
        }
        cmd.exec()?;
        Ok(())
    }

    /// Queried when queuing each unit of work. If it returns true, then the
    /// unit will always be rebuilt, independent of whether it needs to be.
    fn force_rebuild(&self, _unit: &Unit) -> bool {
        true // Overriding the default to force all units to be processed.
    }
}

pub(crate) fn workspace(config: &Config, manifest_path: Option<PathBuf>) -> CargoResult<Workspace> {
    let root = match manifest_path {
        Some(path) => path,
        None => cargo::util::important_paths::find_root_manifest_for_wd(config.cwd())?,
    };
    Workspace::new(&root, config)
}

#[derive(StructOpt, Debug)]
pub struct TrawlArgs {
    #[structopt(long = "build_plan")]
    /// Output a build plan to stdout instead of actually compiling
    build_plan: bool,

    #[structopt(short = "o", value_name = "OUTPUT_FILE_PATH", parse(from_os_str))]
    /// Path to output file
    out_path: PathBuf,

    #[structopt(long = "package", short = "p", value_name = "SPEC")]
    /// Package to be used as the root of the tree
    package: Option<String>,

    #[structopt(long = "features", value_name = "FEATURES")]
    /// Space-separated list of features to activate
    features: Option<String>,

    #[structopt(long = "all-features")]
    /// Activate all available features
    all_features: bool,

    #[structopt(long = "no-default-features")]
    /// Do not activate the `default` feature
    no_default_features: bool,

    #[structopt(long = "target", value_name = "TARGET")]
    /// Set the target triple
    target: Option<String>,

    #[structopt(long = "all-targets")]
    /// Return dependencies for all targets. By default only the host target is matched.
    all_targets: bool,

    #[structopt(
        long = "manifest-path",
        value_name = "MANIFEST_PATH",
        parse(from_os_str)
    )]
    /// Path to Cargo.toml
    manifest_path: Option<PathBuf>,

    #[structopt(long = "invert", short = "i")]
    /// Invert the tree direction
    invert: bool,

    #[structopt(long = "jobs", short = "j")]
    /// Number of parallel jobs, defaults to # of CPUs
    jobs: Option<u32>,

    #[structopt(long = "verbose", short = "v", parse(from_occurrences))]
    /// Use verbose cargo output (-vv very verbose)
    verbose: u32,

    #[structopt(long = "quiet", short = "q")]
    /// Omit cargo output to stdout
    quiet: bool,

    #[structopt(long = "offline")]
    /// cargo offline mode
    offline: bool,

    #[structopt(long = "color", value_name = "WHEN")]
    /// Cargo output coloring: auto, always, never
    color: Option<String>,

    #[structopt(long = "frozen")]
    /// Require Cargo.lock and cache are up to date
    frozen: bool,

    #[structopt(long = "locked")]
    /// Require Cargo.lock is up to date
    locked: bool,

    #[structopt(short = "Z", value_name = "FLAG")]
    /// Unstable (nightly-only) flags to Cargo
    unstable_flags: Vec<String>,

    #[structopt(long = "include-tests")]
    /// Count unsafe usage in tests.
    include_tests: bool,

    #[structopt(long = "build-dependencies", alias = "build-deps")]
    /// Also analyze build dependencies
    build_deps: bool,

    #[structopt(long = "dev-dependencies", alias = "dev-deps")]
    /// Also analyze dev dependencies
    dev_deps: bool,

    #[structopt(long = "all-dependencies", alias = "all-deps")]
    /// Analyze all dependencies, including build and dev
    all_deps: bool,
}

/// Based on code from cargo-bloat. It seems weird that CompileOptions can be
/// constructed without providing all standard cargo options, TODO: Open an issue
/// in cargo?
pub fn build_compile_options<'a>(args: &'a TrawlArgs, config: &'a cargo::Config) -> CompileOptions {
    let features = args
        .features
        .iter()
        .flat_map(|s| s.split_whitespace())
        .flat_map(|s| s.split(','))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let mut opt = CompileOptions::new(&config, CompileMode::Check { test: false }).unwrap();
    opt.features = features.collect::<_>();
    opt.all_features = args.all_features;
    opt.no_default_features = args.no_default_features;

    // BuildConfig, see https://docs.rs/cargo/0.31.0/cargo/core/compiler/struct.BuildConfig.html
    if let Some(jobs) = args.jobs {
        opt.build_config.jobs = jobs;
    }

    opt.build_config.build_plan = args.build_plan;

    opt
}

pub fn real_main(args: &TrawlArgs, config: &mut cargo::Config) -> CliResult {
    let target_dir = None;
    config.configure(
        args.verbose,
        args.quiet,
        args.color.as_ref().map(|s| s.as_str()),
        args.frozen,
        args.locked,
        args.offline,
        &target_dir,
        &args.unstable_flags,
        &[],
    )?;

    let ws = workspace(config, args.manifest_path.clone())?;
    let (packages, _) = cargo::ops::resolve_ws(&ws)?;

    let build_config = config.build_config()?;
    info!("rustc config == {:?}", build_config.rustc);

    let copt = build_compile_options(args, config);
    let rs_files_used_in_compilation = resolve_rs_file_deps(&copt, &ws).unwrap();

    let allow_partial_results = true;
    let include_tests = if args.include_tests {
        ast_walker::IncludeTests::Yes
    } else {
        ast_walker::IncludeTests::No
    };
    let mut out_file =
        std::fs::File::create(&args.out_path).expect("Could not open output file for writing");

    let rs_files_scanned = find_unsafe_in_packages(
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
