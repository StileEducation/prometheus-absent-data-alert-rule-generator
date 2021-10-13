# prometheus-absent-data-alert-rule-generator

`prometheus-absent-data-alert-rule-generator` is a tool to generate alerts for
missing data in time-series that we use in our other alerts and recordings.

## Usage

This is a normal Rust project and can be managed using Cargo. For the most
up-to-date usage, run:

```shell
cargo run -- --help
```

For normal usage you just need to pass in the directory containing your
Prometheus rules files in as the first (and only) positional argument, e.g.

``` shell
cargo run -- ./rules
```

This will generate an `absent.rules.yml` file in the `rules` directory
containing all your absent alerts.


## Absent time-series alert generation

The high-level overview of how the alerts are generated is:

1. Parse all the `*.rules.yml` files in the given directory and pull out the
   expressions from the `expr` field
2. In each expression, pull out all the time-series selector used (e.g.
   `stack:public_http_errors_5xx_non_L3:rate1m_sum` or
   `aws_firehose_delivery_to_redshift_success_minimum[1h]`)
3. Group the selectors into those that are all the same
4. For each group "merge" the selectors into a rule based on some rules
  - "for" field is chosen based on the minimum of all the selectors' origin
    rules with a floor of 1h
4. For each selector generate a rule of the form:
```yaml
- expr: "absent(<selector>)"
  alert: absent_<selector name>
  annotations:
    description: "No data for '<selector>'. This alert rule was generated by   prometheus-absent-data-alert-rule-generator."
    summary: "No data for '<selector>' data"
  for: <chosen for>
  labels:
    severity: business_hours_page
```
**NOTE:** For range-vector selectors (e.g.
`aws_firehose_delivery_to_redshift_success_minimum[1h]`) the `absent_over_time`
function is used because it's the range-vector equivalent of `absent`.

5. Dump all the rules to `absent.rules.yml` in the input directory or to the
   specified output file.

# Ignoring selectors

You can ignore selectors by listing their names, one line per name, in a text
file. By default the text file is `ignore_metrics.txt` in this directory, or you
can use the `--ignore-file` flag to pass in your own path.

# Testing

Testing is done using the normal `cargo test`. The only external dependency that
you need to have installed is `promtool` which you can get from the prometheus
repository.
