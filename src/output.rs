use crate::gateway::protocol::{CredentialUsage, GatewayStatus};
use crate::provider::Provider;

pub(crate) fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub(crate) fn print_gateway_health(status: &GatewayStatus, provider_filter: Option<Provider>) {
    println!("Gateway:   reachable");
    if status.credentials.is_empty() {
        println!("Credentials: auditing");
        return;
    }
    for (name, report) in &status.credentials {
        if provider_filter.is_some_and(|filter| report.provider != Some(filter)) {
            continue;
        }
        let provider = report.provider.map_or("unknown", Provider::name);
        let active = report
            .provider
            .is_some_and(|provider| status.selected.for_provider(provider) == Some(name));
        let audit = if report.usage.is_some() {
            "available"
        } else {
            "unavailable"
        };
        println!(
            "  {name} [{provider}]{}: token {}, audit {audit}",
            if active { " (selected)" } else { "" },
            report.token_state.label()
        );
        if let Some(error) = &report.error {
            println!(
                "    Last error [{}]: {}",
                serde_json::to_value(error.kind)
                    .ok()
                    .and_then(|kind| kind.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "unknown".into()),
                error.message
            );
        }
    }
}

pub(crate) fn format_statusline_segment(status: &GatewayStatus) -> String {
    let mut parts = Vec::new();
    if let Some(selected) = status.selected.claude.as_deref() {
        let usage = status
            .credentials
            .get(selected)
            .and_then(|report| report.usage.as_ref());
        let (five, seven) = match usage {
            Some(CredentialUsage::Claude(usage)) => (
                usage
                    .five_hour
                    .as_ref()
                    .and_then(|window| window.utilization),
                usage
                    .seven_day
                    .as_ref()
                    .and_then(|window| window.utilization),
            ),
            Some(CredentialUsage::Codex(usage)) => (
                usage
                    .rate_limit
                    .primary_window
                    .as_ref()
                    .and_then(|window| window.used_percent),
                usage
                    .rate_limit
                    .secondary_window
                    .as_ref()
                    .and_then(|window| window.used_percent),
            ),
            None => (None, None),
        };

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

    fn parse(value: serde_json::Value) -> GatewayStatus {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn statusline_segment_shows_selected_account_and_usage() {
        let status = parse(serde_json::json!({
            "selected": {"claude": "personal", "codex": null},
            "credentials": {
                "personal": {
                    "provider": "claude",
                    "token_state": "valid",
                    "token_expires_at": null,
                    "usage": {
                        "five_hour": {"utilization": 12.4, "resets_at": null},
                        "seven_day": {"utilization": 34.6, "resets_at": null}
                    },
                    "error": null,
                    "checked_at": 1
                }
            }
        }));
        assert_eq!(
            format_statusline_segment(&status),
            "Subhub: personal | 5h 12% | 7d 35%"
        );
        assert_eq!(
            format_statusline_segment(&parse(serde_json::json!({"selected": {}}))),
            "Subhub: auditing"
        );
    }

    #[test]
    fn statusline_segment_ignores_codex_provider() {
        let status = parse(serde_json::json!({
            "selected": {"claude": "personal", "codex": "work"},
            "credentials": {
                "personal": {
                    "provider": "claude",
                    "token_state": "valid",
                    "token_expires_at": null,
                    "usage": {"five_hour": {"utilization": 12.4, "resets_at": null}},
                    "error": null,
                    "checked_at": 1
                },
                "work": {
                    "provider": "codex",
                    "token_state": "unknown",
                    "token_expires_at": null,
                    "usage": {"rate_limit": {"primary_window": {"used_percent": 55.0, "reset_at": 1}}},
                    "error": null,
                    "checked_at": 1
                }
            }
        }));
        assert_eq!(
            format_statusline_segment(&status),
            "Subhub: personal | 5h 12%"
        );

        let codex_only = parse(serde_json::json!({
            "selected": {"claude": null, "codex": "work"},
            "credentials": {}
        }));
        assert_eq!(format_statusline_segment(&codex_only), "Subhub: auditing");
    }

    #[test]
    fn health_report_round_trips_through_wire_format() {
        let status = parse(serde_json::json!({
            "selected": {"claude": "personal", "codex": null},
            "credentials": {
                "personal": {
                    "provider": "claude",
                    "token_state": "refresh_due",
                    "token_expires_at": 123,
                    "usage": null,
                    "error": {"kind": "transient_audit", "message": "usage request timed out"},
                    "checked_at": 1
                }
            }
        }));
        let report = &status.credentials["personal"];
        assert_eq!(report.provider, Some(Provider::Claude));
        assert_eq!(report.token_state.label(), "refresh_due");
        assert_eq!(
            report.error.as_ref().unwrap().message,
            "usage request timed out"
        );
    }
}
