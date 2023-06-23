use crate::toolchain::LocalToolchain;
use anyhow::Context;
use benchlib::benchmark::passes_filter;
use cargo_metadata::Message;
use core::option::Option;
use core::option::Option::Some;
use core::result::Result::Ok;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Directory containing runtime benchmarks.
/// We measure how long does it take to execute these crates, which is a proxy of the quality
/// of code generated by rustc.
pub fn runtime_benchmark_dir() -> PathBuf {
    PathBuf::from("collector/runtime-benchmarks")
}

/// A binary that defines several benchmarks using the `run_benchmark_group` function from
/// `benchlib`.
#[derive(Debug)]
pub struct BenchmarkGroup {
    pub binary: PathBuf,
    pub benchmark_names: Vec<String>,
}

impl BenchmarkGroup {
    pub fn name(&self) -> &str {
        self.binary.file_name().unwrap().to_str().unwrap()
    }
}

/// A collection of benchmark suites gathered from a directory.
#[derive(Debug)]
pub struct BenchmarkSuite {
    pub groups: Vec<BenchmarkGroup>,
}

impl BenchmarkSuite {
    pub fn total_benchmark_count(&self) -> u64 {
        self.benchmark_names().count() as u64
    }

    pub fn filtered_benchmark_count(&self, filter: &BenchmarkFilter) -> u64 {
        self.benchmark_names()
            .filter(|benchmark| {
                passes_filter(
                    benchmark,
                    filter.exclude.as_deref(),
                    filter.include.as_deref(),
                )
            })
            .count() as u64
    }

    pub fn benchmark_names(&self) -> impl Iterator<Item = &str> {
        self.groups
            .iter()
            .flat_map(|suite| suite.benchmark_names.iter().map(|n| n.as_ref()))
    }
}

pub struct BenchmarkFilter {
    pub exclude: Option<String>,
    pub include: Option<String>,
}

impl BenchmarkFilter {
    pub fn new(exclude: Option<String>, include: Option<String>) -> BenchmarkFilter {
        Self { exclude, include }
    }
}

struct BenchmarkGroupCrate {
    name: String,
    path: PathBuf,
}

/// Find all runtime benchmark crates in `benchmark_dir` and compile them.
/// We assume that each binary defines a benchmark suite using `benchlib`.
/// We then execute each benchmark suite with the `list-benchmarks` command to find out its
/// benchmark names.
pub fn discover_benchmarks(
    toolchain: &LocalToolchain,
    benchmark_dir: &Path,
    target_dir: Option<&Path>,
) -> anyhow::Result<BenchmarkSuite> {
    let benchmark_crates = get_runtime_benchmark_groups(benchmark_dir)?;

    let group_count = benchmark_crates.len();
    println!("Compiling {group_count} runtime benchmark groups");

    let mut groups = Vec::new();
    for (index, benchmark_crate) in benchmark_crates.into_iter().enumerate() {
        let benchmark_target_dir =
            target_dir.map(|dir| dir.join(&benchmark_crate.name).join("target"));

        // Show incremental progress
        print!(
            "\r{}\rCompiling `{}` ({}/{group_count})",
            " ".repeat(80),
            benchmark_crate.name,
            index + 1
        );
        std::io::stdout().flush().unwrap();

        let cargo_process = start_cargo_build(
            toolchain,
            &benchmark_crate.path,
            benchmark_target_dir.as_deref(),
        )
        .with_context(|| {
            anyhow::anyhow!("Cannot not start compilation of {}", benchmark_crate.name)
        })?;
        discover_benchmark_groups(cargo_process, &mut groups).with_context(|| {
            anyhow::anyhow!("Cannot compile runtime benchmark {}", benchmark_crate.name)
        })?;
    }
    println!();

    groups.sort_unstable_by(|a, b| a.binary.cmp(&b.binary));
    log::debug!("Found binaries: {:?}", groups);

    Ok(BenchmarkSuite { groups })
}

/// Locates benchmark binaries compiled by cargo, and then executes them to find out what benchmarks
/// do they contain.
fn discover_benchmark_groups(
    mut cargo_process: Child,
    groups: &mut Vec<BenchmarkGroup>,
) -> anyhow::Result<()> {
    let stream = BufReader::new(cargo_process.stdout.take().unwrap());
    for message in Message::parse_stream(stream) {
        let message = message?;
        match message {
            Message::CompilerArtifact(artifact) => {
                if let Some(ref executable) = artifact.executable {
                    // Found a binary compiled by a runtime benchmark crate.
                    // Execute it so that we find all the benchmarks it contains.
                    if artifact.target.kind.iter().any(|k| k == "bin") {
                        let path = executable.as_std_path().to_path_buf();
                        let benchmarks = gather_benchmarks(&path).map_err(|err| {
                            anyhow::anyhow!(
                                "Cannot gather benchmarks from `{}`: {err:?}",
                                path.display()
                            )
                        })?;
                        log::info!("Compiled {}", path.display());
                        groups.push(BenchmarkGroup {
                            binary: path,
                            benchmark_names: benchmarks,
                        });
                    }
                }
            }
            Message::TextLine(line) => println!("{}", line),
            Message::CompilerMessage(msg) => {
                print!("{}", msg.message.rendered.unwrap_or(msg.message.message))
            }
            _ => {}
        }
    }
    let output = cargo_process.wait()?;
    if !output.success() {
        Err(anyhow::anyhow!(
            "Failed to compile runtime benchmark, exit code {}",
            output.code().unwrap_or(1)
        ))
    } else {
        Ok(())
    }
}

/// Starts the compilation of a single runtime benchmark crate.
/// Returns the stdout output stream of Cargo.
fn start_cargo_build(
    toolchain: &LocalToolchain,
    benchmark_dir: &Path,
    target_dir: Option<&Path>,
) -> anyhow::Result<Child> {
    let mut command = Command::new(&toolchain.cargo);
    command
        .env("RUSTC", &toolchain.rustc)
        .arg("build")
        .arg("--release")
        .arg("--message-format")
        .arg("json-diagnostic-rendered-ansi")
        .current_dir(benchmark_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    if let Some(target_dir) = target_dir {
        command.arg("--target-dir");
        command.arg(target_dir);
    }

    let child = command
        .spawn()
        .map_err(|error| anyhow::anyhow!("Failed to start cargo: {:?}", error))?;
    Ok(child)
}

/// Uses a command from `benchlib` to find the benchmark names from the given
/// benchmark binary.
fn gather_benchmarks(binary: &Path) -> anyhow::Result<Vec<String>> {
    let output = Command::new(binary).arg("list").output()?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// Finds all runtime benchmarks (crates) in the given directory.
fn get_runtime_benchmark_groups(directory: &Path) -> anyhow::Result<Vec<BenchmarkGroupCrate>> {
    let mut groups = Vec::new();
    for entry in std::fs::read_dir(directory).with_context(|| {
        anyhow::anyhow!("Failed to list benchmark dir '{}'", directory.display())
    })? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() || !path.join("Cargo.toml").is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| anyhow::anyhow!("Cannot get filename of {}", path.display()))?
            .to_string();

        groups.push(BenchmarkGroupCrate { name, path });
    }
    groups.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(groups)
}
