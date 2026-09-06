//! Shared execution seam for non-interactive CLI providers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::subprocess::{run_blocking, SubprocessError, SubprocessOptions, SubprocessResult};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) fn environment() -> HashMap<String, String> {
    let mut environment: HashMap<String, String> = std::env::vars().collect();
    environment.insert("NO_COLOR".into(), "1".into());
    environment.insert("TERM".into(), "dumb".into());
    environment
}

pub(crate) fn resolve_binary(
    name: &str,
    override_key: &str,
    environment: &HashMap<String, String>,
) -> Option<PathBuf> {
    if let Some(path) = environment
        .get(override_key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    {
        return Some(path);
    }

    let path = environment
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))?
        .1
        .as_str();
    let extensions: Vec<String> = environment
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATHEXT"))
        .map(|(_, value)| {
            value
                .split(';')
                .filter(|value| !value.is_empty())
                .map(|value| value.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_else(|| vec![".exe".into(), ".com".into()]);

    for directory in std::env::split_paths(path) {
        let direct = directory.join(name);
        if direct.is_file() && is_native_executable(&direct, &extensions) {
            return Some(direct);
        }
        if Path::new(name).extension().is_none() {
            for extension in &extensions {
                let candidate = directory.join(format!("{name}{extension}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn is_native_executable(path: &Path, extensions: &[String]) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| {
            extensions
                .iter()
                .any(|ext| ext[1..].eq_ignore_ascii_case(value))
        })
        .unwrap_or(false)
}

pub(crate) async fn run(
    binary: PathBuf,
    arguments: Vec<String>,
    environment: HashMap<String, String>,
) -> Result<SubprocessResult, SubprocessError> {
    tokio::task::spawn_blocking(move || {
        run_blocking(
            binary,
            &arguments,
            SubprocessOptions::new(environment, COMMAND_TIMEOUT),
        )
    })
    .await
    .map_err(|error| SubprocessError::LaunchFailed(format!("CLI task panicked: {error}")))?
}

#[cfg(all(test, windows))]
pub(crate) fn fixture_binary() -> PathBuf {
    use std::sync::OnceLock;

    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE
        .get_or_init(|| {
            let directory = std::env::temp_dir().join(format!(
                "codexbar-cli-fixture-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&directory).expect("create CLI fixture directory");
            let source = directory.join("main.rs");
            let executable = directory.join("codexbar-cli-fixture.exe");
            std::fs::write(
                &source,
                r##"fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["usage"] => println!("Signed in as fixture@example.com\nAmp Free: 75% remaining today (resets daily)"),
        ["account", "status"] => println!("319,054 credits remaining Max Plan\n450,000 credits / month\n9 days remaining in this billing cycle (ends 6/9/2026)"),
        ["configure", "export-credentials", "--profile", _, "--format", "process"] => {
            println!(r#"{{"AccessKeyId":"AKID","SecretAccessKey":"secret","SessionToken":"token"}}"#)
        }
        _ => std::process::exit(64),
    }
}"##,
            )
            .expect("write CLI fixture source");
            let status = std::process::Command::new("rustc")
                .arg("--crate-name")
                .arg("codexbar_cli_fixture")
                .arg(&source)
                .arg("-o")
                .arg(&executable)
                .status()
                .expect("launch rustc for CLI fixture");
            assert!(status.success(), "compile CLI fixture");
            executable
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_must_point_to_a_real_file() {
        let mut environment = HashMap::new();
        environment.insert("TOOL_PATH".into(), "Z:\\missing\\tool.exe".into());
        assert_eq!(resolve_binary("tool", "TOOL_PATH", &environment), None);
    }
}
