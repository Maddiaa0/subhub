use crate::provider_name;
use serde_json::Value;

pub(crate) fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub(crate) fn print_gateway_health(status: &Value, provider_filter: Option<crate::Provider>) {
    println!("Gateway:   reachable");
    let selected = status.get("selected").and_then(Value::as_object);
    let credentials = status.get("credentials").and_then(Value::as_object);
    let Some(credentials) = credentials else {
        println!("Credentials: auditing");
        return;
    };
    for (name, health) in credentials {
        let provider = health
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if provider_filter.is_some_and(|filter| provider_name(filter) != provider) {
            continue;
        }
        let active = selected
            .and_then(|selected| selected.get(provider))
            .and_then(Value::as_str)
            == Some(name);
        let token = health
            .get("token_state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let audit = if health.get("usage").is_some_and(|usage| !usage.is_null()) {
            "available"
        } else {
            "unavailable"
        };
        println!(
            "  {name} [{provider}]{}: token {token}, audit {audit}",
            if active { " (selected)" } else { "" }
        );
        if let Some(error) = health.get("error").and_then(Value::as_object) {
            let kind = error
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let message = error.get("message").and_then(Value::as_str).unwrap_or("");
            println!("    Last error [{kind}]: {message}");
        }
    }
}

pub(crate) fn format_statusline_segment(status: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(selected) = status.pointer("/selected/claude").and_then(Value::as_str) {
        let usage = status
            .get("credentials")
            .and_then(|credentials| credentials.get(selected))
            .and_then(|credential| credential.get("usage"));
        let five = usage
            .and_then(|usage| usage.get("five_hour"))
            .and_then(|window| window.get("utilization"))
            .and_then(Value::as_f64)
            .or_else(|| {
                usage
                    .and_then(|usage| usage.pointer("/rate_limit/primary_window/used_percent"))
                    .and_then(Value::as_f64)
            });
        let seven = usage
            .and_then(|usage| usage.get("seven_day"))
            .and_then(|window| window.get("utilization"))
            .and_then(Value::as_f64)
            .or_else(|| {
                usage
                    .and_then(|usage| usage.pointer("/rate_limit/secondary_window/used_percent"))
                    .and_then(Value::as_f64)
            });

        parts.push(format!("Subhub: {selected}"));
        if let Some(five) = five {
            parts.push(format!("5h {five:.0}%"));
        }
        if let Some(seven) = seven {
            parts.push(format!("7d {seven:.0}%"));
        }
    }
    if parts.is_empty() {
        "Subhub: auditing".into()
    } else {
        parts.join(" | ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statusline_segment_shows_selected_account_and_usage() {
        let status = serde_json::json!({
            "selected": {"claude": "personal", "codex": null},
            "credentials": {
                "personal": {
                    "usage": {
                        "five_hour": {"utilization": 12.4},
                        "seven_day": {"utilization": 34.6}
                    }
                }
            }
        });
        assert_eq!(
            format_statusline_segment(&status),
            "Subhub: personal | 5h 12% | 7d 35%"
        );
        assert_eq!(
            format_statusline_segment(&serde_json::json!({"selected": {}})),
            "Subhub: auditing"
        );
    }

    #[test]
    fn statusline_segment_ignores_codex_provider() {
        let status = serde_json::json!({
            "selected": {"claude": "personal", "codex": "work"},
            "credentials": {
                "personal": {
                    "usage": {"five_hour": {"utilization": 12.4}}
                },
                "work": {
                    "usage": {"rate_limit": {"primary_window": {"used_percent": 55.0}}}
                }
            }
        });
        assert_eq!(
            format_statusline_segment(&status),
            "Subhub: personal | 5h 12%"
        );

        let codex_only = serde_json::json!({
            "selected": {"claude": null, "codex": "work"},
            "credentials": {
                "work": {
                    "usage": {"rate_limit": {"primary_window": {"used_percent": 55.0}}}
                }
            }
        });
        assert_eq!(format_statusline_segment(&codex_only), "Subhub: auditing");
    }
}
