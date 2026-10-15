//! Validation of a test report against `schema/test-game.schema.json`.
//!
//! The types mirror the schema and reject any property it does not list. What serde cannot
//! express, such as minimum lengths, ranges, date-times and the rules that tie one field to
//! another, is checked after deserialising. Keep both in step with the schema.

// Some fields are deserialised only to check their type.
#![allow(dead_code)]

use anyhow::{Result, bail, ensure};
use chrono::DateTime;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    schema_version: u64,
    mode: Mode,
    status: Status,
    summary: String,
    game_url: Option<String>,
    started_at: String,
    duration_ms: u64,
    balance_before: Option<String>,
    balance_after: Option<String>,
    video_path: Option<String>,
    trace_path: Option<String>,
    steps: Vec<Step>,
    error: Option<Error>,
    environment: Option<Environment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    RealMoney,
    NoBalance,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Working,
    Failing,
    TestFailed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    action: Option<String>,
    description: String,
    selector: Option<String>,
    status: StepStatus,
    error: Option<Error>,
    #[serde(default)]
    screenshots: Vec<Screenshot>,
    #[serde(default)]
    console_messages: Vec<ConsoleMessage>,
    #[serde(default)]
    failed_requests: Vec<FailedRequest>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StepStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Screenshot {
    path: String,
    caption: Option<String>,
    taken_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsoleMessage {
    level: ConsoleLevel,
    text: String,
    source_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConsoleLevel {
    Warning,
    Error,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FailedRequest {
    url: String,
    method: String,
    status_code: Option<u64>,
    failure: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Error {
    message: String,
    stack: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Environment {
    browser: Option<Browser>,
    viewport: Option<Viewport>,
    user_agent: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Browser {
    Chromium,
    Firefox,
    Webkit,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Viewport {
    width: u64,
    height: u64,
}

/// Checks that `report` follows the schema.
///
/// # Errors
///
/// Fails, naming the field, at the first place where it does not.
pub fn validate(report: &Value) -> Result<()> {
    let report = Report::deserialize(report)?;
    ensure!(report.schema_version == 2, "schema_version must be 2");
    non_empty("summary", &report.summary)?;
    date_time("started_at", &report.started_at)?;
    for (name, value) in [
        ("game_url", &report.game_url),
        ("balance_before", &report.balance_before),
        ("balance_after", &report.balance_after),
        ("video_path", &report.video_path),
        ("trace_path", &report.trace_path),
    ] {
        if let Some(value) = value {
            non_empty(name, value)?;
        }
    }
    match (&report.status, &report.error) {
        (Status::TestFailed, None) => bail!("a test_failed report needs an error"),
        (Status::Working | Status::Failing, Some(_)) => {
            bail!("only a test_failed report may have an error")
        }
        (_, Some(error)) => non_empty("error.message", &error.message)?,
        _ => {}
    }
    for step in &report.steps {
        check_step(step)?;
    }
    if let Some(Viewport { width, height }) = report.environment.and_then(|env| env.viewport) {
        ensure!(
            width >= 1 && height >= 1,
            "the viewport must be at least 1×1"
        );
    }
    Ok(())
}

fn check_step(step: &Step) -> Result<()> {
    non_empty("step description", &step.description)?;
    if let Some(action) = &step.action {
        non_empty("step action", action)?;
    }
    match &step.error {
        Some(error) => non_empty("step error.message", &error.message)?,
        None if step.status == StepStatus::Failed => bail!("a failed step needs an error"),
        None => {}
    }
    for screenshot in &step.screenshots {
        non_empty("screenshot path", &screenshot.path)?;
        if let Some(taken_at) = &screenshot.taken_at {
            date_time("screenshot taken_at", taken_at)?;
        }
    }
    for request in &step.failed_requests {
        non_empty("failed request url", &request.url)?;
        non_empty("failed request method", &request.method)?;
        match (request.status_code, &request.failure) {
            (Some(code), _) => ensure!(
                (400..=599).contains(&code),
                "failed request status_code {code} is not from 400 to 599"
            ),
            (None, Some(failure)) => non_empty("failed request failure", failure)?,
            (None, None) => bail!("a failed request needs a status_code or a failure"),
        }
    }
    Ok(())
}

fn non_empty(name: &str, value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "{name} is empty");
    Ok(())
}

fn date_time(name: &str, value: &str) -> Result<()> {
    ensure!(
        DateTime::parse_from_rfc3339(value).is_ok(),
        "{name} is not an RFC 3339 date-time"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn report() -> Value {
        json!({
            "schema_version": 2,
            "mode": "no_balance",
            "status": "failing",
            "summary": "No insufficient-balance message.",
            "game_url": "https://www.betsson.es/casino",
            "started_at": "2026-10-12T10:00:00.000Z",
            "duration_ms": 1200,
            "balance_before": "0,00 €",
            "balance_after": "0,00 €",
            "video_path": "/data/assets/x/session.webm",
            "steps": [{
                "action": "click",
                "description": "Pressed spin once",
                "status": "failed",
                "error": { "message": "No message appeared" },
                "screenshots": [{ "path": "/data/assets/x/screenshots/1.png", "caption": "Lobby" }],
                "console_messages": [{ "level": "error", "text": "boom", "source_url": "https://x" }],
                "failed_requests": [{ "url": "https://x", "method": "GET", "status_code": 404 }]
            }],
            "environment": {
                "browser": "chromium",
                "viewport": { "width": 1440, "height": 900 },
                "user_agent": "Mozilla/5.0"
            }
        })
    }

    #[test]
    fn accepts_a_report_that_follows_the_schema() {
        validate(&report()).unwrap();
    }

    #[test]
    fn rejects_what_the_schema_does_not_allow() {
        for (pointer, value) in [
            ("/schema_version", json!(1)),
            ("/mode", json!("demo")),
            ("/mode", Value::Null),
            ("/balance_before", json!("")),
            ("/status", json!("passed")),
            ("/summary", json!("")),
            ("/started_at", json!("yesterday")),
            ("/duration_ms", json!(-1)),
            ("/duration_ms", json!(1.5)),
            ("/error", json!({ "message": "only on test_failed" })),
            ("/extra", json!(true)),
            ("/steps/0/error", Value::Null),
            ("/steps/0/status", json!("ok")),
            ("/steps/0/console_messages/0/level", json!("info")),
            ("/steps/0/failed_requests/0/status_code", json!(0)),
            ("/steps/0/screenshots/0/taken_at", json!("now")),
            ("/environment/browser", json!("chrome")),
            ("/environment/viewport/width", json!(0)),
        ] {
            let mut broken = report();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            let parent = broken.pointer_mut(parent).unwrap().as_object_mut().unwrap();
            if value.is_null() {
                parent.remove(key);
            } else {
                parent.insert(key.to_owned(), value.clone());
            }
            assert!(
                validate(&broken).is_err(),
                "{pointer} = {value} was accepted"
            );
        }
    }

    #[test]
    fn a_test_failed_report_needs_an_error() {
        let mut broken = report();
        broken["status"] = json!("test_failed");
        assert!(validate(&broken).is_err());
        broken["error"] = json!({ "message": "The browser did not start" });
        validate(&broken).unwrap();
    }
}
