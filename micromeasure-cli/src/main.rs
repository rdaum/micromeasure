// Copyright 2026 Ryan Daum
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use clap::{Args, Parser, Subcommand};
use micromeasure::{
    ComparisonExitStatus, ComparisonOptions, DEFAULT_MAXIMUM_CV_PERCENT,
    DEFAULT_MAXIMUM_OUTLIER_FRACTION, DEFAULT_MINIMUM_CHANGE_PERCENT, RegressionPolicy,
    ReportDocumentType, SuiteComparisonError, compare_report_inputs, validate_report_input,
};
use std::{
    error::Error,
    ffi::OsString,
    fmt, fs,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

#[derive(Debug, Parser)]
#[command(
    name = "micromeasure",
    version,
    about = "Validate and compare micromeasure report artifacts"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compare one report pair or two directories of reports.
    Compare(CompareArgs),
    /// Validate one report or every JSON report beneath a directory.
    Validate(ValidateArgs),
}

#[derive(Debug, Args)]
struct CompareArgs {
    /// Current report file or directory.
    #[arg(long)]
    current: PathBuf,

    /// Baseline report file or directory.
    #[arg(long)]
    baseline: PathBuf,

    /// Write the complete combined analysis as pretty JSON.
    #[arg(long)]
    json_output: Option<PathBuf>,

    /// Write the combined human summary as Markdown.
    #[arg(long)]
    markdown_output: Option<PathBuf>,

    /// Material change threshold in percent.
    #[arg(long, default_value_t = DEFAULT_MINIMUM_CHANGE_PERCENT)]
    minimum_change: f64,

    /// Maximum coefficient of variation before reporting instability.
    #[arg(long, default_value_t = DEFAULT_MAXIMUM_CV_PERCENT)]
    maximum_cv: f64,

    /// Maximum outlier fraction from 0.0 through 1.0.
    #[arg(long, default_value_t = DEFAULT_MAXIMUM_OUTLIER_FRACTION)]
    maximum_outlier_fraction: f64,

    /// Return status 1 when a material regression is found.
    #[arg(long)]
    fail_on_regression: bool,

    /// Return status 1 when a matched result is marked invalid.
    #[arg(long)]
    fail_on_invalid: bool,

    /// Require every matched suite to have identical case sets.
    #[arg(long)]
    strict_result_set: bool,

    /// Require directory inputs to have identical suite sets.
    #[arg(long)]
    strict_suite_set: bool,

    /// Explicitly justify comparing different runners or environments.
    #[arg(long)]
    environment_override: Option<String>,
}

#[derive(Debug, Args)]
struct ValidateArgs {
    /// Report file or directory to validate.
    path: PathBuf,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let status = ExitCode::from(error.exit_code() as u8);
            let _ = error.print();
            return status;
        }
    };
    match run(cli) {
        Ok(status) => exit_code(status),
        Err(error) => {
            eprintln!("micromeasure: {error}");
            exit_code(ComparisonExitStatus::Error)
        }
    }
}

fn run(cli: Cli) -> Result<ComparisonExitStatus, CliError> {
    match cli.command {
        Command::Compare(arguments) => compare(arguments),
        Command::Validate(arguments) => validate(arguments),
    }
}

fn compare(arguments: CompareArgs) -> Result<ComparisonExitStatus, CliError> {
    reject_conflicting_outputs(
        arguments.json_output.as_deref(),
        arguments.markdown_output.as_deref(),
    )?;

    let mut options = ComparisonOptions::default()
        .allow_partial_result_set(!arguments.strict_result_set)
        .require_same_suite_set(arguments.strict_suite_set);
    if let Some(reason) = arguments.environment_override {
        options = options.with_environment_override(reason);
    }
    let policy = RegressionPolicy::advisory()
        .with_minimum_change_percent(arguments.minimum_change)
        .with_maximum_cv_percent(Some(arguments.maximum_cv))
        .with_maximum_outlier_fraction(Some(arguments.maximum_outlier_fraction))
        .fail_on_regression(arguments.fail_on_regression)
        .fail_on_invalid(arguments.fail_on_invalid);
    let analysis = compare_report_inputs(arguments.current, arguments.baseline, &options, &policy)?;

    if let Some(path) = arguments.json_output {
        let json = analysis.render_json_pretty()?;
        write_atomic(&path, json.as_bytes())?;
    }
    if let Some(path) = arguments.markdown_output {
        let markdown = analysis.render_markdown();
        write_atomic(&path, markdown.as_bytes())?;
    }
    io::stdout()
        .write_all(analysis.render_terminal().as_bytes())
        .map_err(CliError::Stdout)?;
    Ok(analysis.exit_status())
}

fn validate(arguments: ValidateArgs) -> Result<ComparisonExitStatus, CliError> {
    let reports = validate_report_input(&arguments.path)?;
    let mut output = format!(
        "validated {} report{} from {}\n",
        reports.len(),
        if reports.len() == 1 { "" } else { "s" },
        arguments.path.display()
    );
    for reference in reports {
        output.push_str(&format!(
            "  {} [{} {}]\n",
            reference.suite.as_deref().unwrap_or("<missing>"),
            document_type_name(reference.document_type),
            reference.content_digest
        ));
    }
    io::stdout()
        .write_all(output.as_bytes())
        .map_err(CliError::Stdout)?;
    Ok(ComparisonExitStatus::Success)
}

fn reject_conflicting_outputs(
    json_output: Option<&Path>,
    markdown_output: Option<&Path>,
) -> Result<(), CliError> {
    if let (Some(json), Some(markdown)) = (json_output, markdown_output)
        && json == markdown
    {
        return Err(CliError::ConflictingOutputs {
            path: json.to_path_buf(),
        });
    }
    Ok(())
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), CliError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent).map_err(|source| CliError::Output {
            path: path.to_path_buf(),
            source,
        })?;
    }
    let directory = parent.unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| CliError::InvalidOutputPath {
            path: path.to_path_buf(),
        })?;

    for attempt in 0..100_u32 {
        let mut temporary_name = OsString::from(".");
        temporary_name.push(file_name);
        temporary_name.push(format!(".tmp-{}-{attempt}", std::process::id()));
        let temporary_path = directory.join(temporary_name);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(CliError::Output {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let result = file.write_all(contents).and_then(|()| file.sync_all());
        drop(file);
        let result = result.and_then(|()| fs::rename(&temporary_path, path));
        if let Err(source) = result {
            let _ = fs::remove_file(&temporary_path);
            return Err(CliError::Output {
                path: path.to_path_buf(),
                source,
            });
        }
        return Ok(());
    }

    Err(CliError::Output {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a temporary output file",
        ),
    })
}

fn document_type_name(document_type: ReportDocumentType) -> &'static str {
    match document_type {
        ReportDocumentType::NativeBenchmark => "native_benchmark",
        ReportDocumentType::Series => "series",
        _ => "unknown",
    }
}

fn exit_code(status: ComparisonExitStatus) -> ExitCode {
    ExitCode::from(status.code() as u8)
}

#[derive(Debug)]
enum CliError {
    Suite(SuiteComparisonError),
    Json(serde_json::Error),
    ConflictingOutputs { path: PathBuf },
    InvalidOutputPath { path: PathBuf },
    Output { path: PathBuf, source: io::Error },
    Stdout(io::Error),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Suite(source) => source.fmt(formatter),
            Self::Json(source) => {
                write!(formatter, "failed to serialize comparison JSON: {source}")
            }
            Self::ConflictingOutputs { path } => write!(
                formatter,
                "JSON and Markdown outputs must use different paths, both were {}",
                path.display()
            ),
            Self::InvalidOutputPath { path } => {
                write!(formatter, "invalid output path {}", path.display())
            }
            Self::Output { path, source } => {
                write!(formatter, "failed to write {}: {source}", path.display())
            }
            Self::Stdout(source) => write!(formatter, "failed to write terminal output: {source}"),
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Suite(source) => Some(source),
            Self::Json(source) => Some(source),
            Self::Output { source, .. } | Self::Stdout(source) => Some(source),
            Self::ConflictingOutputs { .. } | Self::InvalidOutputPath { .. } => None,
        }
    }
}

impl From<SuiteComparisonError> for CliError {
    fn from(source: SuiteComparisonError) -> Self {
        Self::Suite(source)
    }
}

impl From<serde_json::Error> for CliError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json(source)
    }
}
