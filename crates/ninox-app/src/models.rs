//! Model-picker options + live `models_cmd` discovery (spec §V "Model
//! pickers ... are selects, not free text", fed in precedence order:
//! discovery → known_models → configured value, then `custom…` last).

use ninox_core::harness::HarnessSpec;

/// The picker's escape hatch — selecting it reveals a mono free-text input.
pub const CUSTOM_SENTINEL: &str = "custom…";

pub fn model_options(
    spec:       &HarnessSpec,
    discovered: Option<&[String]>,
    configured: Option<&str>,
) -> Vec<String> {
    let mut opts: Vec<String> = match discovered {
        Some(d) if !d.is_empty() => d.to_vec(),
        _                        => spec.known_models.clone(),
    };
    if let Some(c) = configured {
        if !c.is_empty() && !opts.iter().any(|o| o == c) {
            opts.push(c.to_string());
        }
    }
    opts.push(CUSTOM_SENTINEL.to_string());
    opts
}

pub fn parse_models_output(stdout: &str) -> Vec<String> {
    stdout.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

/// Run a spec's `models_cmd` argv. Warn-and-fall-through: any failure
/// (missing binary, non-zero exit, empty output) returns `None` and the
/// picker falls back to `known_models`.
pub async fn run_models_cmd(cmd: Vec<String>) -> Option<Vec<String>> {
    let (bin, args) = cmd.split_first()?;
    let out = match tokio::process::Command::new(bin).args(args).output().await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("models_cmd {bin} failed to launch: {e}");
            return None;
        }
    };
    if !out.status.success() {
        tracing::warn!("models_cmd {bin} exited with {}", out.status);
        return None;
    }
    let models = parse_models_output(&String::from_utf8_lossy(&out.stdout));
    (!models.is_empty()).then_some(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::harness::HarnessSpec;

    fn spec(known: &[&str]) -> HarnessSpec {
        HarnessSpec { known_models: known.iter().map(|s| s.to_string()).collect(),
                      ..HarnessSpec::default() }
    }

    #[test]
    fn discovered_models_win_over_known() {
        let d = vec!["live-1".to_string(), "live-2".to_string()];
        assert_eq!(model_options(&spec(&["k-1"]), Some(&d), None),
                   vec!["live-1", "live-2", CUSTOM_SENTINEL]);
    }

    #[test]
    fn empty_discovery_falls_through_to_known_models() {
        assert_eq!(model_options(&spec(&["k-1", "k-2"]), Some(&[]), None),
                   vec!["k-1", "k-2", CUSTOM_SENTINEL]);
        assert_eq!(model_options(&spec(&["k-1"]), None, None),
                   vec!["k-1", CUSTOM_SENTINEL]);
    }

    #[test]
    fn configured_value_is_always_present() {
        assert_eq!(model_options(&spec(&["k-1"]), None, Some("mine")),
                   vec!["k-1", "mine", CUSTOM_SENTINEL]);
        // ...but not duplicated when already listed
        assert_eq!(model_options(&spec(&["k-1"]), None, Some("k-1")),
                   vec!["k-1", CUSTOM_SENTINEL]);
    }

    #[test]
    fn custom_is_always_last_even_with_nothing_else() {
        assert_eq!(model_options(&spec(&[]), None, None), vec![CUSTOM_SENTINEL]);
    }

    #[test]
    fn parse_models_output_takes_trimmed_nonempty_lines() {
        assert_eq!(parse_models_output("  a-model \n\nb-model\n"),
                   vec!["a-model", "b-model"]);
    }
}
