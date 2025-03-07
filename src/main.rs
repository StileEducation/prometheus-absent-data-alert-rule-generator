//! prometheus-absent-data-alert-rule-generator is a tool that parses all the
//! Prometheus rules in a specified directory and generates a rules file with
//! alerts for when any of the rules used are absent.
use std::{
    cmp::max,
    collections::BTreeMap,
    fs,
    path::{self, Path},
    vec,
};

use anyhow::{ensure, Context, Result};
use itertools::Itertools;
use path::PathBuf;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

const USAGE: &str = "
prometheus-absent-data-alert-rule-generator [OPTIONS] <PATH>

ARGS:
    PATH            Path to the directory containing the Prometheus rules files.

OPTIONS:
    -h, --help      Print this help information.
    --dry-run       Dry run. Don't output the generated rules files.
    --output-file   File to write the absent rules to. Defaults to absent.rules.yml in <PATH>.
    --ignore-file   Path to the file with a list of metrics to ignore. Defaults to ignore_metrics.txt in cargo path.
    --playbook-link Link to the playbook to associate with all generated alerts. If not provided no playbook is associated.
";

/// A little helper for making [BTreeMap]'s nicer to write. This lets you use
/// something similar to Ruby's Hash syntax:
///
/// ```
/// use std::collections::BTreeMap;
///
/// let btree: BtreeMap<String, String> = btree_map! {
///     "key" => "value",
///     "other_key" => "value"
/// };
/// println!("{:?}", btree);
///````
///
/// Note that you can't have a trailing "," after the
/// last argument.
macro_rules! btree_map {
    { $($key:expr => $value:expr), +} => {
        {
            let mut btree = BTreeMap::new();
            $(btree.insert($key.into(), $value.into());)+
            btree
        }
    };
}

/// Top level of Prometheus rules files.
#[derive(Deserialize, Serialize, Debug)]
struct PrometheusRulesConfig {
    groups: Vec<PrometheusRuleGroup>,
}

/// A group of Prometheus rules.
#[derive(Deserialize, Serialize, Debug)]
struct PrometheusRuleGroup {
    /// The name of the group.
    name: String,
    /// Rules contained within the group.
    rules: Vec<PrometheusRule>,
}

/// A Prometheus rule. Every rule _most_ have the `expr` field but some of the
/// others change depending on the rule type (e.g. alert vs record) so we
/// they're stored in an unstructured way in `untyped_fields`.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
struct PrometheusRule {
    /// The rule expression.
    expr: String,
    /// Any other fields in the rule. Uses a [BTreeMap] because it has an
    /// ordering, unlike [HashMap].
    #[serde(flatten)]
    untyped_fields: BTreeMap<String, serde_yaml::Value>,
}

/// Representation of an alert rule for an absent selector.
///
/// This is mostly to just wrap it up an allow us to implement [Into]
/// [PrometheusRule] which has the logic.
struct PrometheusAbsentSelectorAlertRule {
    name: String,
    expr: String,
    selector_expr: String,
    r#for: prometheus_parser::PromDuration,
    labels: BTreeMap<String, String>,
}

impl From<PrometheusAbsentSelectorAlertRule> for PrometheusRule {
    fn from(p: PrometheusAbsentSelectorAlertRule) -> PrometheusRule {
        // If someone is seeing this alert somewhere that isn't the
        // `absent.rules.yml` file they'll probably be surprised by the name.
        // Explain exactly what this is alerting for and that is was generated,
        // not written by someone with extensive Java experience.
        let tool_name = env!("CARGO_PKG_NAME");
        let annotations: BTreeMap<String, String> = btree_map! {
            "summary" => format!("No data for '{}'", p.selector_expr),
            "description" => format!("No data for '{}'. This alert rule was generated by {}.", p.selector_expr, tool_name)
        };

        let annotations_mapping: serde_yaml::Mapping = btree_to_yaml_mapping(annotations);
        let labels_mapping = btree_to_yaml_mapping(p.labels);

        PrometheusRule {
            expr: p.expr,
            untyped_fields: btree_map! {
                "alert" => p.name,
                // Don't alert the instant a time series is missing, give a bit of
                // leeway.
                "for" => p.r#for.to_string(),
                "annotations" => annotations_mapping,
                "labels" => labels_mapping
            },
        }
    }
}

/// Representation of a Prometheus selector that contains the [PrometheusRule]
/// that it came from and the [prometheus_parser::Selector].
#[derive(Clone)]
struct SelectorWithOriginRule {
    selector: prometheus_parser::Selector,
    rule: PrometheusRule,
}

impl SelectorWithOriginRule {
    /// Key to sort and group [SelectorWithOriginRule] by.
    ///
    /// It is just the string representation of the selector's
    /// [prometheus_parser::Selector] as it is something that we want to
    /// eventually be unique and already implements ord.
    fn sort_key(&self) -> String {
        // Don't care about the `span` field
        // as that will be different for everything.
        prometheus_parser::Selector {
            span: None,
            ..self.selector.clone()
        }
        .to_string()
    }
}

/// Available command line options. See [parse_options] where [pico_args] is used
/// to parse the provided command line options into this struct.
struct Opts {
    rules_dir: PathBuf,
    output_file: PathBuf,
    dry_run: bool,
    ignore_file: PathBuf,
    playbook_link: Option<String>,
}

fn main() -> Result<()> {
    env_logger::init();
    let opts = parse_options()?;
    process_rules_dir(
        opts.rules_dir,
        opts.output_file,
        Some(opts.ignore_file),
        opts.playbook_link,
        opts.dry_run,
    )?;
    Ok(())
}

/// Process the given rules directory, outputting the absent rules file to
/// `output_file`.
///
/// This just wraps things up so we can easily call them in a unit test, [main]
/// just passes through the command line options.
fn process_rules_dir<P: AsRef<Path>>(
    rules_dir: P,
    output_file: P,
    ignore_file: Option<P>,
    playbook_link: Option<String>,
    dry_run: bool,
) -> Result<()> {
    log::debug!(
        "Reading rules from {}, outputting rules to {}",
        rules_dir.as_ref().display(),
        output_file.as_ref().display(),
    );
    if dry_run {
        log::info!("This is a dry run, no files will be generated");
    }
    let rules_file_matcher = format!("{}/**/*.rules.yml", rules_dir.as_ref().display());
    let metrics_to_ignore: Vec<String> = if let Some(file) = ignore_file {
        load_ignore_file(file)?
    } else {
        vec![]
    };
    log::debug!("Ignoring these metrics {:?}", metrics_to_ignore);

    // We only want to write the file out if all is well but it's useful to run
    // through the whole thing so we can pick up as many issues as possible in a
    // single run. `failure` is used as a flag to tell us if there has been a
    // failure or not but doesn't interrupt the processing of other rules.
    let mut failure = false;
    let rule_files: Vec<PathBuf> = glob::glob(&rules_file_matcher)?
        .filter_map(|path| match path {
            Ok(p) => Some(p),
            Err(e) => {
                log::error!("Failed to read path: {}", e);
                failure = true;
                None
            }
        })
        .sorted_by(|left, right| left.cmp(right))
        .collect();

    // Get a list of _all_ the selectors we use.
    let selectors: Vec<SelectorWithOriginRule> = rule_files
        .iter()
        .flat_map(|path| {
            // If the output file is already there ignore it. We're going to
            // overwrite it at the end. Use `canoncialize` to handle all the
            // edge cases around expanding paths and such. It's pretty far
            // fetched that it'll actually fail but it's easy enough to handle.
            // Things will probably fail down the line if cannicalization did
            // fail so log the failure here but still have a go at getting the
            // selectors. If `output_file` doesn't exist then `canonicalize`
            // will fail so we need to check it does exists before check if it
            // is the same as the path we're currently looking at. We don't need
            // to check `path` because it was given to us by [glob::glob] so it
            // must exist.
            let path_is_output_file = output_file.as_ref().exists()
                && match (fs::canonicalize(path), fs::canonicalize(&output_file)) {
                    (Ok(canonical_path), Ok(canonical_output_file)) => {
                        canonical_path == canonical_output_file
                    }
                    (Ok(_), Err(e)) => {
                        log::error!("Failed to canonicalize output file path: {}", e);
                        failure = true;
                        false
                    }
                    (Err(e), Ok(_)) => {
                        log::error!("Failed to canonicalize path: {}", e);
                        failure = true;
                        false
                    }
                    (Err(path_e), Err(output_file_e)) => {
                        log::error!("Failed to canonicalize output file path: {}", path_e);
                        log::error!("Failed to canonicalize path: {}", output_file_e);
                        failure = true;
                        false
                    }
                };
            if path_is_output_file {
                vec![]
            } else {
                match get_selectors_in_file(path) {
                    Ok(selectors) => selectors,
                    Err(e) => {
                        log::error!("Failed to get selectors from file: {}", e);
                        failure = true;
                        vec![]
                    }
                }
            }
        })
        .collect();
    let grouped_selectors: Vec<(String, Vec<SelectorWithOriginRule>)> = selectors
        .iter()
        .sorted_by_key(|selector| selector.sort_key())
        .group_by(|selector| selector.sort_key())
        .into_iter()
        .filter_map(|(selector, group)| {
            if metrics_to_ignore.contains(&selector) {
                None
            } else {
                Some((selector, group.cloned().collect()))
            }
        })
        .collect();
    log::info!(
        "Found {} unique selectors in {} files",
        grouped_selectors.len(),
        rule_files.len()
    );
    let absent_alert_rules = grouped_selectors
        .iter()
        .map(|(_selector, selectors)| merge_selectors_into_rule(selectors, playbook_link.clone()))
        .collect();
    let config = PrometheusRulesConfig {
        groups: vec![PrometheusRuleGroup {
            name: "absent_label_alerts".into(),
            rules: absent_alert_rules,
        }],
    };
    log::debug!(
        "Writing generated absent selector rules config to {}",
        output_file.as_ref().display()
    );
    ensure!(!failure, "Failure at some point during the generation process. See logs above for more details. Config file not being written out.");
    write_generated_config_to_file(output_file, &config)?;
    Ok(())
}

/// Merge the given [Selector]s into a [PrometheusRule].
///
/// This is where the logic for adopting certain attributes from the selector
/// origin rules is contained. Currently we do this for the "for" field, where
/// we take the smallest "for" then use it or 1h, whichever is larger.
fn merge_selectors_into_rule(
    selectors: &[SelectorWithOriginRule],
    playbook_link: Option<String>,
) -> PrometheusRule {
    let name = build_absent_selector_alert_name(&selectors.first().unwrap().selector);
    let function = wrap_selector_in_absent(&selectors.first().unwrap().selector);
    let shortest_for = selectors
        .iter()
        .flat_map(|s| {
            s.rule
                .untyped_fields
                .get("for")
                .and_then(|val| val.as_str())
                .and_then(|duration| {
                    if duration.len() < 2 {
                        log::error!(
                            "Malformed duration, expected at least two characters, found '{}'",
                            duration
                        );
                        return None;
                    }
                    let unit = duration[duration.len() - 1..].into();
                    match duration[0..duration.len() - 1].parse() {
                        Ok(value) => {
                            match prometheus_parser::PromDuration::from_pair(unit, value) {
                                Ok(duration) => Some(duration),
                                Err(e) => {
                                    log::error!("Invalid duration {}{}: {}", value, unit, e);
                                    None
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("Invalid 'for' field '{}': {}", duration, e);
                            None
                        }
                    }
                })
        })
        .min();
    let chosen_for = shortest_for
        .map(|duration| max(duration, prometheus_parser::PromDuration::Hours(1)))
        .unwrap_or(prometheus_parser::PromDuration::Hours(1));
    let mut labels: BTreeMap<String, String> = btree_map! {
            "severity" => "business_hours_page",
            "how_much_should_you_panic" => "Not much (1/3)"
    };
    if let Some(playbook_link) = playbook_link {
        labels.insert("playbook".to_string(), playbook_link);
    }

    PrometheusAbsentSelectorAlertRule {
        name,
        expr: function.to_string(),
        selector_expr: selectors.first().unwrap().selector.to_string(),
        r#for: chosen_for,
        labels,
    }
    .into()
}

/// Build the alert name for a selector.
///
/// This takes the metric name, labels, range, and offset, and smashes them
/// together separated by underscores and puts "absent_" in front. For complex
/// selectors the results will _not_ be pretty but at least it'll be somewhat
/// clear what it's for (not some random id) and will only contain allowed
/// characters ([a-zA-Z_][a-zA-Z0-9_]*).
fn build_absent_selector_alert_name(selector: &prometheus_parser::Selector) -> String {
    let metric = if let Some(metric) = &selector.metric {
        format!("_{}", metric)
    } else {
        // This should never happen and I think it's a problem with
        // prometheus_parser's data model. Just log it and make the first char
        // something that is allowed.
        log::error!("Found selector with no metric: '{}'", selector);
        "_".into()
    };
    let mut labels = selector
        .labels
        .iter()
        .map(|label| {
            // LabelOp's string repr is the symbol which isn't compatible with
            // the allowed characters for alert names. Lets convert it to
            // something that still has meaning but is allowed.
            let op = match label.op {
                prometheus_parser::LabelOp::Equal => "equal",
                prometheus_parser::LabelOp::NotEqual => "notequal",
                prometheus_parser::LabelOp::RegexEqual => "regexequal",
                prometheus_parser::LabelOp::RegexNotEqual => "regexnotequal",
            };
            // This regex is constant so panicing on it being incorrect is okay
            // as it would be a developer error.
            let not_allowed_chars_re = Regex::new("[^a-zA-Z0-9_:]").expect("invalid regex");
            let value = not_allowed_chars_re.replace_all(&label.value, "_");
            format!("{}_{}_{}", label.key, op, value)
        })
        .join("_");
    if !labels.is_empty() {
        labels = "_".to_string() + &labels
    }
    let range = if let Some(range) = selector.range {
        format!("_{}", range)
    } else {
        "".into()
    };
    let offset = if let Some(offset_duration) = selector.offset {
        format!("_offset_{}", offset_duration)
    } else {
        "".into()
    };
    format!("absent{}{}{}{}", metric, labels, range, offset)
}

/// Write out the serializable config to the provided file with a comment header
/// to say this generated.
fn write_generated_config_to_file<P: AsRef<Path>, C: Serialize>(path: P, config: &C) -> Result<()> {
    let serialized = serde_yaml::to_string(config)?;
    let contents = format!(
        "
# DO NOT MODIFY THIS FILE BY HAND. It was generated by {} in operations/tools/prometheus-absent-data-alert-rule-generator.
{}",
        env!("CARGO_PKG_NAME"), serialized
    );
    Ok(fs::write(path, contents)?)
}

fn get_selectors_in_file<P: AsRef<Path>>(rules_path: P) -> Result<Vec<SelectorWithOriginRule>> {
    let config = load_rules_from_file(&rules_path)?;
    let mut selectors: Vec<SelectorWithOriginRule> = vec![];
    let mut failed = false;
    for group in config.groups {
        for rule in group.rules {
            let expr_selectors = match prometheus_parser::parse_expr(&rule.expr) {
                Ok(expr) => get_selectors_from_expression(&expr),
                Err(e) => {
                    log::error!("Failed to parse expression '{}': {}", rule.expr, e);
                    failed = true;
                    continue;
                }
            };
            let mut rule_selectors: Vec<SelectorWithOriginRule> = expr_selectors
                .into_iter()
                .map(|selector| SelectorWithOriginRule {
                    selector,
                    rule: rule.clone(),
                })
                .collect();
            selectors.append(&mut rule_selectors);
            // Also explicitly get the recordings we've defined. Even if
            // they're not used in other Prometheus rules they may be used
            // in places like Grafana. We've defined them for a reason so we
            // should alert if they're missing.
            if let Some(record_name_value) = rule.untyped_fields.get("record") {
                let maybe_record_name = record_name_value.as_str();
                if let Some(record_name) = maybe_record_name {
                    match prometheus_parser::parse_expr(record_name) {
                        Ok(prometheus_parser::Expression::Selector(selector)) => {
                            selectors.push(SelectorWithOriginRule {
                                selector,
                                rule: rule.clone(),
                            });
                        }
                        Ok(_) => {
                            log::error!("Expected record name '{}' to be a selector", record_name);
                            failed = true;
                        }
                        Err(e) => {
                            log::error!("Failed to parse selector name '{}': {}", record_name, e);
                            failed = true;
                        }
                    }
                }
            }
        }
    }
    if failed {
        anyhow::bail!(
            "There was a failure getting selectors from {}, see logs for details.",
            rules_path.as_ref().display()
        )
    }
    Ok(selectors)
}

/// Get all the selectors in an expression.
///
/// Recursively traverse the AST and return all the selectors it finds.
fn get_selectors_from_expression(
    expr: &prometheus_parser::Expression,
) -> Vec<prometheus_parser::Selector> {
    match expr {
        prometheus_parser::Expression::Float(_) => vec![],
        prometheus_parser::Expression::String(_) => vec![],
        prometheus_parser::Expression::Selector(selector) => vec![selector.to_owned()],
        prometheus_parser::Expression::Group(prometheus_parser::Group { expression, .. }) => {
            get_selectors_from_expression(expression)
        }
        prometheus_parser::Expression::Function(function) => function
            .args
            .iter()
            .flat_map(|arg| get_selectors_from_expression(arg))
            .collect(),
        prometheus_parser::Expression::Operator(operator) => {
            let mut selectors = get_selectors_from_expression(&operator.lhs);
            selectors.extend(get_selectors_from_expression(&operator.rhs));
            selectors
        }
        prometheus_parser::Expression::BoolOperator(bool_operator) => {
            let mut selectors = get_selectors_from_expression(&bool_operator.lhs);
            selectors.extend(get_selectors_from_expression(&bool_operator.rhs));
            selectors
        }
    }
}

fn load_rules_from_file<P: AsRef<Path>>(rules_path: P) -> Result<PrometheusRulesConfig> {
    let content = fs::read_to_string(&rules_path).context(format!(
        "Failed to read the rules file at {}",
        rules_path.as_ref().display()
    ))?;
    let config = serde_yaml::from_str(&content)?;
    Ok(config)
}

/// Load the lines from an "ignore file", skipping comment lines.
fn load_ignore_file<P: AsRef<Path>>(ignore_file: P) -> Result<Vec<String>> {
    let contents = fs::read_to_string(&ignore_file).context(format!(
        "Failed to read the ignore file at '{}'",
        ignore_file.as_ref().display()
    ))?;
    let ignore_lines = contents
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.trim().starts_with('#'))
        .collect();
    Ok(ignore_lines)
}

/// Parse the provided command line options into [Opts].
fn parse_options() -> Result<Opts> {
    let mut args = pico_args::Arguments::from_env();
    if args.contains(["-h", "--help"]) {
        println!("{}", USAGE);
        std::process::exit(1);
    }
    let dry_run = args.contains("--dry-run");
    let maybe_output_file: Option<PathBuf> = args.opt_value_from_str("--output-file")?;
    let ignore_file: PathBuf = args
        .opt_value_from_str("--ignore-file")?
        .unwrap_or_else(|| {
            let mut path = PathBuf::new();
            path = path.join(env!("CARGO_MANIFEST_DIR"));
            path.join("ignore_metrics.txt")
        });
    let playbook_link = args.opt_value_from_str("--playbook-link")?;
    let rules_dir: PathBuf = args.free_from_str()?;
    let opts = Opts {
        dry_run,
        output_file: maybe_output_file.unwrap_or_else(|| rules_dir.join("absent.rules.yml")),
        rules_dir,
        ignore_file,
        playbook_link,
    };
    let remaining = args.finish();
    if !remaining.is_empty() {
        log::warn!("Ignoring junk: {:?}", remaining);
    }
    Ok(opts)
}

/// Wrap the given [prometheus_parser::Expression] in the applicable absent
/// function.
///
/// Prometheus has two functions in the absent family, `absent` and
/// `absent_over_time`
/// (https://prometheus.io/docs/prometheus/latest/querying/functions/#absent).
/// `absent` expects an instant-vector selector and `absent_over_time` expects a
/// range-vector selector. We can easily differentiate between the two in
/// [prometheus_parser]'s AST because the [prometheus_parser::Selector] struct
/// will have a `range` if it is a range-vector selector and otherwise it's an
/// instant-vector.
fn wrap_selector_in_absent(selector: &prometheus_parser::Selector) -> prometheus_parser::Function {
    let function_name = if selector.range.is_some() {
        "absent_over_time"
    } else {
        "absent"
    };
    prometheus_parser::Function::new(function_name).arg(selector.clone().wrap())
}

/// Converting a BTreeMap to a serde_yaml::Value turns out to be a massive pain.
/// The best I could find is converting it to an intermediate Mapping here. You
/// can't convert a BTreeMap directly to a mapping, instead you need an Iterator
/// with an Item type of (Value, Value). Hence the shenanigans below.
fn btree_to_yaml_mapping<K: Into<Value> + Clone, V: Into<Value> + Clone>(
    btree: BTreeMap<K, V>,
) -> serde_yaml::Mapping {
    btree
        .into_iter()
        .map(|(key, value)| -> (Value, Value) { (key.into(), value.into()) })
        .collect()
}

#[cfg(test)]
mod test {
    use anyhow::Result;
    use pretty_assertions::assert_eq;
    use xshell::{cmd, Shell};

    use super::*;

    fn temp_file() -> Result<String> {
        let tmp_file = tempfile::NamedTempFile::new()?;
        // Unlink so we can write to it in this process but noone else can use it.
        std::fs::remove_file(&tmp_file)?;
        Ok(tmp_file.path().to_str().unwrap().to_string())
    }

    #[test]
    fn test_wrap_selector_in_absent() {
        let expr_and_expected = vec![
            (
                "stack:public_http_errors_5xx_non_L3:rate1m_sum",
                "absent(stack:public_http_errors_5xx_non_L3:rate1m_sum)",
            ),
            (
                r#"publicapi_http_errors_5xx_count{is_load_shedding!="true",slo="L1"}[30s]"#,
                r#"absent_over_time(publicapi_http_errors_5xx_count{is_load_shedding!="true",slo="L1"}[30s])"#,
            ),
        ];
        for (expr, expected_expr) in expr_and_expected {
            let selector = if let prometheus_parser::Expression::Selector(s) =
                prometheus_parser::parse_expr(expr).expect("failed to parse expression")
            {
                s
            } else {
                panic!("Expressions must be a selector");
            };
            let wrapped_in_absent = wrap_selector_in_absent(&selector);
            // Make sure it produces valid syntax.
            prometheus_parser::parse_expr(&wrapped_in_absent.to_string())
                .expect("wrap_in_absent produce an invalid expression");
            assert_eq!(wrapped_in_absent.to_string(), expected_expr);
        }
    }

    #[test]
    fn test_get_selectors_from_file() {
        let file_name = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/test_get_selectors_from_file.yml"
        );
        let actual_selectors: Vec<String> = get_selectors_in_file(file_name)
            .expect("failed to get selectors from file")
            .iter()
            .map(|it| it.selector.to_string())
            .sorted()
            .collect();
        let mut expected_selectors = vec![
            r#"node_load1{box_type="data-warehouse"}"#,
            "a_recording:cpu",
            r#"node_cpu_seconds_total{mode!="idle"}[1m]"#,
        ];
        expected_selectors.sort_unstable();
        assert_eq!(actual_selectors, expected_selectors);
    }

    #[test]
    fn test_get_selectors_from_expression() {
        let expr_and_expected = vec![
            (
                "stack:public_http_errors_5xx_non_L3:rate1m_sum",
                vec!["stack:public_http_errors_5xx_non_L3:rate1m_sum"],
            ),
            (
                r#"publicapi_http_errors_5xx_count{is_load_shedding!="true",slo="L1"}[30s]"#,
                vec![r#"publicapi_http_errors_5xx_count{is_load_shedding!="true",slo="L1"}[30s]"#],
            ),
            ("(month() > bool 9) + (month() < bool 4)", vec![]),
            (
                r#"count(max by(stack_id) (up{job="rabbitmq"} == 1))"#,
                vec![r#"up{job="rabbitmq"}"#],
            ),
            (
                r#"up{job="aws_rds"} == 1 unless aws_rds_free_storage_space_minimum{dbinstance_identifier=~"live-db-.\\d"}"#,
                vec![
                    r#"up{job="aws_rds"}"#,
                    r#"aws_rds_free_storage_space_minimum{dbinstance_identifier=~"live-db-.\\d"}"#,
                ],
            ),
            (
                r#"sum(irate(publicapi_http_request_count[30s])) by (stack_id, slo, route, method) and on(stack_id) slb_live_stack_number{slb="prod"} == 1"#,
                vec![
                    "publicapi_http_request_count[30s]",
                    r#"slb_live_stack_number{slb="prod"}"#,
                ],
            ),
        ];
        for (expr, expected_selectors) in expr_and_expected {
            let parsed = prometheus_parser::parse_expr(expr).expect("failed to parse expression");
            let selectors: Vec<String> = get_selectors_from_expression(&parsed)
                .iter()
                .map(|s| s.to_string())
                .collect();
            assert_eq!(selectors, expected_selectors);
        }
    }

    #[test]
    fn test_build_absent_selector_alert_name() {
        let expr_and_expected = vec![
            ("stile_log_messages_logged_count{level=~\"error|fatal\",client_sent!=\"true\"}[15m]", "absent_stile_log_messages_logged_count_level_regexequal_error_fatal_client_sent_notequal_true_15m"),
            ("stack:error_log:rate15m_sum", "absent_stack:error_log:rate15m_sum"),
            ("publicapi_http_errors_5xx_count{is_load_shedding!=\"true\",is_internal_admin=\"false\",slo!=\"L3\"}[1m]", "absent_publicapi_http_errors_5xx_count_is_load_shedding_notequal_true_is_internal_admin_equal_false_slo_notequal_L3_1m"),
            ("publicapi_http_response_time_bucket[1m]", "absent_publicapi_http_response_time_bucket_1m"),
            (r#"aws_elasticache_evictions_maximum{cache_cluster_id=~"prod-redis-shard-.*"}"#, "absent_aws_elasticache_evictions_maximum_cache_cluster_id_regexequal_prod_redis_shard___")
        ];
        for (expr, expected_name) in expr_and_expected {
            let selector = if let prometheus_parser::Expression::Selector(s) =
                prometheus_parser::parse_expr(expr).expect("failed to parse expression")
            {
                s
            } else {
                panic!("Expressions must be a selector");
            };
            let name = build_absent_selector_alert_name(&selector);
            assert_eq!(name, expected_name);
        }
    }

    #[test]
    fn test_merge_selectors_into_rule() {
        let selectors = vec![
            SelectorWithOriginRule {
                selector: prometheus_parser::Selector {
                    metric: Some("some_metric".into()),
                    ..Default::default()
                },
                rule: PrometheusRule {
                    expr: "some_metric".into(),
                    untyped_fields: btree_map! {
                        "for" => "1h"
                    },
                },
            },
            SelectorWithOriginRule {
                selector: prometheus_parser::Selector {
                    metric: Some("some_metric".into()),
                    ..Default::default()
                },
                rule: PrometheusRule {
                    expr: "some_metric".into(),
                    untyped_fields: btree_map! {
                        "for" => "5h"
                    },
                },
            },
        ];
        let expected_rule: PrometheusRule = PrometheusAbsentSelectorAlertRule {
            name: "absent_some_metric".into(),
            expr: "absent(some_metric)".into(),
            selector_expr: "some_metric".into(),
            r#for: prometheus_parser::PromDuration::Hours(1),
            labels: btree_map! {
                "severity" => "business_hours_page",
                "how_much_should_you_panic" => "Not much (1/3)"
            },
        }
        .into();
        let actual_rule = merge_selectors_into_rule(&selectors, None);
        assert_eq!(actual_rule, expected_rule);
    }

    #[test]
    fn test_merge_selectors_into_rule_min_1h() {
        let playbook_link = "test".to_string();
        let selectors = vec![
            SelectorWithOriginRule {
                selector: prometheus_parser::Selector {
                    metric: Some("some_metric".into()),
                    ..Default::default()
                },
                rule: PrometheusRule {
                    expr: "some_metric".into(),
                    untyped_fields: btree_map! {
                        "for" => "1m"
                    },
                },
            },
            SelectorWithOriginRule {
                selector: prometheus_parser::Selector {
                    metric: Some("some_metric".into()),
                    ..Default::default()
                },
                rule: PrometheusRule {
                    expr: "some_metric".into(),
                    untyped_fields: btree_map! {
                        "for" => "30s"
                    },
                },
            },
        ];
        let expected_rule: PrometheusRule = PrometheusAbsentSelectorAlertRule {
            name: "absent_some_metric".into(),
            expr: "absent(some_metric)".into(),
            selector_expr: "some_metric".into(),
            r#for: prometheus_parser::PromDuration::Hours(1),
            labels: btree_map! {
                "severity" => "business_hours_page",
                "how_much_should_you_panic" => "Not much (1/3)",
                "playbook" => "test"
            },
        }
        .into();
        let actual_rule = merge_selectors_into_rule(&selectors, Some(playbook_link));
        assert_eq!(actual_rule, expected_rule);
    }

    #[test]
    fn test_prometheus_rule_from_prometheus_absent_selector_alert_rule() {
        let rule: PrometheusRule = PrometheusAbsentSelectorAlertRule {
            expr: "absent(some_expr)".into(),
            r#for: prometheus_parser::PromDuration::Hours(1),
            name: "this_thing".into(),
            selector_expr: "some_expr".into(),
            labels: btree_map! {
                "severity" => "business_hours_page",
                "how_much_should_you_panic" => "Not much (1/3)"
            },
        }
        .into();
        let annotations: BTreeMap<String, String> = btree_map! {
            "description" => "No data for 'some_expr'. This alert rule was generated by prometheus-absent-data-alert-rule-generator.",
            "summary" => "No data for 'some_expr'"
        };
        let labels: BTreeMap<String, String> = btree_map! {
            "severity" => "business_hours_page",
            "how_much_should_you_panic" => "Not much (1/3)"
        };
        let expected_rule = PrometheusRule {
            expr: "absent(some_expr)".into(),
            untyped_fields: btree_map! {
                "for" => "1h",
                "alert" => "this_thing",
                "annotations" => btree_to_yaml_mapping(annotations),
                "labels" => btree_to_yaml_mapping(labels)
            },
        };
        assert_eq!(rule, expected_rule);
    }

    #[test]
    fn generates_no_files_on_dry_run() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let output_file = temp_file().expect("failed to get temp file");
        process_rules_dir(
            &format!("{}/alerts", manifest_dir),
            &output_file,
            None,
            None,
            true,
        )
        .expect("failed to process alerts");
        let generated_files =
            glob::glob(&format!("{}/*", output_file)).expect("failed to glob temp dir");
        assert_eq!(generated_files.count(), 0);
    }

    #[test]
    fn generates_valid_rules_file() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let output_file = temp_file().expect("failed to get temp file");
        process_rules_dir(
            format!("{}/alerts", manifest_dir),
            output_file.clone(),
            None,
            None,
            false,
        )
        .expect("failed to process alerts");
        let sh = Shell::new().unwrap();
        cmd!(sh, "promtool check rules {output_file}")
            .run()
            .expect("promtool check failed");
    }

    #[test]
    fn outputs_rules_in_the_same_order() {
        let fixtures_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/alerts");
        let output_file = temp_file().expect("failed to get temp file");
        process_rules_dir(fixtures_dir, &output_file, None, None, false)
            .expect("failed to process fixtures");
        let second_output_file = temp_file().expect("failed to get temp file");
        process_rules_dir(fixtures_dir, &second_output_file, None, None, false)
            .expect("failed to process fixtures");
        let output_file_contents =
            fs::read_to_string(output_file).expect("failed to read output file");
        let second_output_file_contents =
            fs::read_to_string(second_output_file).expect("failed to read second output file");
        assert_eq!(output_file_contents, second_output_file_contents);
    }
}
