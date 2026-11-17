//! Standalone entry point for the same Rust producer used by jit-dev.
use reknot::jit::builder::{build, BuildOptions};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;

const HELP: &str = "reknot-build --manifest-path PATH --package NAME --support-target NAME --cache-dir PATH --output PATH

Build a checked layered COFF/ELF object without PowerShell or Python.
Requires rustup, Cargo, a platform linker and a logic/support workspace.
The output path must be new. The manifest is written to PATH.json.

Options: --toolchain NAME --features a,b --no-default-features --offline
         --target-dir PATH --foundation-manifest PATH --max-cache-bytes N
         --optimization 0..3 --debug-info 0|1|2|line-tables-only
         --support-optimization 0..3|inherit (default: 1; logic default: 0)
";

fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<BuildOptions>, String> {
    let mut args = args.into_iter();
    let mut values = HashMap::new();
    let mut switches = HashSet::new();
    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|_| "option names must be UTF-8")?;
        if arg == "--help" || arg == "-h" {
            return Ok(None);
        }
        if ["--offline", "--no-default-features"].contains(&arg.as_str()) {
            if !switches.insert(arg.clone()) {
                return Err(format!("repeated option: {arg}"));
            }
            continue;
        }
        if ![
            "--manifest-path",
            "--package",
            "--support-target",
            "--cache-dir",
            "--output",
            "--toolchain",
            "--features",
            "--target-dir",
            "--foundation-manifest",
            "--max-cache-bytes",
            "--optimization",
            "--support-optimization",
            "--debug-info",
        ]
        .contains(&arg.as_str())
        {
            return Err(format!("unknown option: {arg}"));
        }
        let value = args.next().ok_or_else(|| format!("{arg} needs a value"))?;
        if values.insert(arg.clone(), value).is_some() {
            return Err(format!("repeated option: {arg}"));
        }
    }
    let mut take = |key: &str| {
        values
            .remove(key)
            .ok_or_else(|| format!("{key} is required; see --help"))
    };
    let mut opts = BuildOptions::new(
        PathBuf::from(take("--manifest-path")?),
        take("--package")?
            .into_string()
            .map_err(|_| "package must be UTF-8")?,
        take("--support-target")?
            .into_string()
            .map_err(|_| "support target must be UTF-8")?,
        PathBuf::from(take("--cache-dir")?),
        PathBuf::from(take("--output")?),
    );
    opts.offline = switches.contains("--offline");
    opts.no_default_features = switches.contains("--no-default-features");
    for (key, value) in values {
        match key.as_str() {
            "--target-dir" => opts.target_directory = value.into(),
            "--foundation-manifest" => opts.foundation_manifest = Some(value.into()),
            _ => {
                let value = value
                    .into_string()
                    .map_err(|_| format!("{key} must be UTF-8"))?;
                match key.as_str() {
                    "--toolchain" => opts.toolchain = value,
                    "--features" => {
                        opts.features = value
                            .split(',')
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .collect()
                    }
                    "--optimization" => {
                        opts.optimization = value.parse().map_err(|_| "invalid optimization")?
                    }
                    "--support-optimization" => {
                        opts.support_optimization = if value == "inherit" {
                            None
                        } else {
                            Some(value.parse().map_err(|_| "invalid support optimization")?)
                        }
                    }
                    "--debug-info" => opts.debug_info = value,
                    "--max-cache-bytes" => {
                        opts.max_cache_bytes = value.parse().map_err(|_| "invalid cache limit")?
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
    Ok(Some(opts))
}
fn main() {
    let result = parse(std::env::args_os().skip(1)).and_then(|options| match options {
        None => {
            print!("{HELP}");
            Ok(())
        }
        Some(options) => build(&options)
            .map(|result| println!("{}", serde_json::to_string_pretty(&result).unwrap()))
            .map_err(|e| e.to_string()),
    });
    if let Err(error) = result {
        eprintln!("reknot-build: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn support_profile_defaults_and_explicit_inheritance() {
        let base = [
            "--manifest-path",
            "Cargo.toml",
            "--package",
            "logic",
            "--support-target",
            "support",
            "--cache-dir",
            "cache",
            "--output",
            "out.obj",
        ];
        let parse_with = |extra: &[&str]| parse(base.iter().chain(extra).map(OsString::from));
        let defaults = parse_with(&[]).unwrap().unwrap();
        assert_eq!(defaults.support_optimization, Some(1));
        assert_eq!(defaults.optimization, 0);
        assert_eq!(
            parse_with(&["--support-optimization", "inherit"])
                .unwrap()
                .unwrap()
                .support_optimization,
            None
        );
        assert_eq!(
            parse_with(&["--support-optimization", "0"])
                .unwrap()
                .unwrap()
                .support_optimization,
            Some(0)
        );
        assert!(parse_with(&["--support-optimization", "fast"]).is_err());
        assert!(
            parse_with(&["--support-optimization", "1", "--support-optimization", "0"]).is_err()
        );
    }
}
