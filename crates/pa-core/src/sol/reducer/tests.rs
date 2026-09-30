use std::sync::Mutex;

use super::receipt::{
    validate_receipt, EvidenceKind, ReceiptRejection, ReceiptStatus, VerifiedEvidence,
    REDUCER_RECEIPT_SCHEMA,
};
use super::*;

fn log() -> String {
    format!(
        "{}FAILED test/a.py::test_x - AssertionError: 1 != 2\n1 failed, 399 passed\n",
        "collecting ... ok\n".repeat(400)
    )
}

const FAILED_LINE: &str = "FAILED test/a.py::test_x - AssertionError: 1 != 2";

type Reply = Box<dyn Fn(&ReducerRequest) -> String + Send + Sync>;

fn scripted(reply: Reply) -> (ReducerCallFn, Arc<Mutex<Vec<ReducerRequest>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    let reply = Arc::new(reply);
    let call: ReducerCallFn = Arc::new(move |request| {
        let text = reply(&request);
        seen.lock().unwrap().push(request);
        Box::pin(async move {
            Ok(ReducerResponse {
                provider: "prov".to_string(),
                model: "small".to_string(),
                ok: true,
                stop_reason: "stop".to_string(),
                error_message: None,
                output_text: text,
                usage: ReducerUsage {
                    input: 10,
                    output: 5,
                    total_tokens: 15,
                },
            })
        })
    });
    (call, calls)
}

fn source_hash(request: &ReducerRequest) -> String {
    request
        .input
        .lines()
        .find_map(|line| line.strip_prefix("source_sha256="))
        .unwrap()
        .to_string()
}

fn receipt(quote: &'static str, status: &'static str) -> Reply {
    Box::new(move |request| {
        json!({
            "schema": REDUCER_RECEIPT_SCHEMA, "source_sha256": source_hash(request),
            "status": status, "uncertain": false,
            "evidence": [{ "kind": "failure", "quote": quote }],
        })
        .to_string()
    })
}

async fn run(
    reducer: &EvidenceReducer,
    code: &str,
    body: &str,
    is_error: bool,
) -> Option<AfterToolCallResult> {
    let args = json!({ "code": code });
    let content = vec![ToolResultContent::text(body)];
    reducer
        .reduce(ToolCallOutcome {
            tool_name: "ipython",
            args: &args,
            content: &content,
            details: &Value::Null,
            is_error,
        })
        .await
}

fn only_text(result: &AfterToolCallResult) -> &str {
    match result.content.as_deref() {
        Some([ToolResultContent::Text(text)]) => &text.text,
        other => panic!("expected one text block, got {other:?}"),
    }
}

#[tokio::test]
async fn a_large_test_log_becomes_a_verified_receipt_pointing_at_the_exact_archive() {
    let dir = tempfile::tempdir().unwrap();
    let (call, calls) = scripted(receipt(FAILED_LINE, "failure"));
    let reducer = EvidenceReducer::new(dir.path(), call);
    let body = log();
    let result = run(
        &reducer,
        r#"r = await bash("pytest -q"); print(r.output)"#,
        &body,
        false,
    )
    .await
    .expect("receipt applied");
    let text = only_text(&result);
    assert!(text.starts_with(receipt::REDUCER_RECEIPT_PREFIX));
    assert!(text.contains("status=failure"));
    assert!(text.contains(&format!(
        "line=401 quote_sha256={}",
        sha256_hex(FAILED_LINE.as_bytes())
    )));
    assert!(text.contains(&format!(
        "quote={}",
        serde_json::to_string(FAILED_LINE).unwrap()
    )));
    assert!(text.len() < body.len());
    let artifact = text
        .lines()
        .find_map(|line| line.strip_prefix("source_artifact="))
        .unwrap();
    assert_eq!(std::fs::read_to_string(artifact).unwrap(), body);
    assert!(text.contains(&format!("source_sha256={}", sha256_hex(body.as_bytes()))));
    assert_eq!(
        calls.lock().unwrap()[0].max_output_tokens,
        DEFAULT_MAX_OUTPUT_TOKENS
    );
    let details = result.details.unwrap();
    assert_eq!(
        details["evidencePreservingReducer"]["evidenceCount"],
        json!(1)
    );
    let journal =
        std::fs::read_to_string(dir.path().join("evidence-preserving-reducer/journal.jsonl"))
            .unwrap();
    let kinds: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["kind"].clone())
        .collect();
    assert_eq!(
        kinds,
        [
            json!("candidate"),
            json!("provider_response"),
            json!("applied")
        ]
    );
}

#[tokio::test]
async fn non_candidates_are_left_untouched_without_calling_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let (call, calls) = scripted(receipt("x", "failure"));
    let reducer = EvidenceReducer::new(dir.path(), call);
    let secret_log = format!("{}API_KEY=sk-live-1234\n", log());
    let cases = [
        (r#"print(open("big.txt").read())"#, log()),
        ("!pytest -q", "1 passed\n".to_string()),
        ("!pytest -q", secret_log),
    ];
    for (code, body) in cases {
        assert!(run(&reducer, code, &body, false).await.is_none(), "{code}");
    }
    assert!(calls.lock().unwrap().is_empty());
    let args = json!({ "command": "pytest" });
    let content = vec![ToolResultContent::text(log())];
    let bash = reducer
        .reduce(ToolCallOutcome {
            tool_name: "bash",
            args: &args,
            content: &content,
            details: &Value::Null,
            is_error: false,
        })
        .await;
    assert!(bash.is_none());
}

#[tokio::test]
async fn unverifiable_receipts_fall_back_to_the_raw_log() {
    let cases: Vec<(Reply, bool)> = vec![
        (receipt("FAILED test/b.py - invented", "failure"), false),
        (receipt("1 failed, 399 passed", "success"), true),
        (Box::new(|_| "the tests failed".to_string()), false),
    ];
    for (reply, is_error) in cases {
        let dir = tempfile::tempdir().unwrap();
        let (call, _) = scripted(reply);
        let reducer = EvidenceReducer::new(dir.path(), call);
        assert!(run(&reducer, "!pytest -q", &log(), is_error)
            .await
            .is_none());
    }
}

#[tokio::test]
async fn a_fenced_failure_receipt_is_accepted_for_a_cell_that_did_not_raise() {
    let dir = tempfile::tempdir().unwrap();
    let inner = receipt("1 failed, 399 passed", "failure");
    let (call, _) = scripted(Box::new(move |request| {
        format!("```json\n{}\n```", inner(request))
    }));
    let reducer = EvidenceReducer::new(dir.path(), call);
    let result = run(&reducer, "!npm test", &log(), false).await.unwrap();
    assert!(only_text(&result).contains("status=failure"));
}

fn archive_for(body: &str) -> (tempfile::TempDir, Archive) {
    let dir = tempfile::tempdir().unwrap();
    let archive = archive_body(dir.path(), body).unwrap();
    (dir, archive)
}

#[test]
fn quotes_are_verified_byte_for_byte_and_deduplicated() {
    let body = log();
    let (_dir, archive) = archive_for(&body);
    let raw = |evidence: Value, status: &str| {
        json!({ "schema": REDUCER_RECEIPT_SCHEMA, "source_sha256": archive.hash, "status": status, "uncertain": true, "evidence": evidence }).to_string()
    };
    let duplicate = json!([
        { "kind": "failure", "quote": FAILED_LINE },
        { "kind": "failure", "quote": FAILED_LINE },
        { "kind": "summary", "quote": "1 failed, 399 passed" },
    ]);
    assert_eq!(
        validate_receipt(&raw(duplicate, "failure"), &archive, &body, false),
        Ok(receipt::ValidatedReceipt {
            status: ReceiptStatus::Failure,
            uncertain: true,
            evidence: vec![
                VerifiedEvidence {
                    kind: EvidenceKind::Failure,
                    line: 401,
                    quote: FAILED_LINE.to_string(),
                    quote_sha256: sha256_hex(FAILED_LINE.as_bytes()),
                },
                VerifiedEvidence {
                    kind: EvidenceKind::Summary,
                    line: 402,
                    quote: "1 failed, 399 passed".to_string(),
                    quote_sha256: sha256_hex(b"1 failed, 399 passed"),
                },
            ],
        })
    );
    let near_miss = json!([{ "kind": "failure", "quote": "FAILED test/a.py::test_x - AssertionError: 1 != 3" }]);
    assert_eq!(
        validate_receipt(&raw(near_miss, "failure"), &archive, &body, false),
        Err(ReceiptRejection::UnverifiableQuote)
    );
    let summary_only = json!([{ "kind": "summary", "quote": "1 failed, 399 passed" }]);
    assert_eq!(
        validate_receipt(&raw(summary_only, "failure"), &archive, &body, false),
        Err(ReceiptRejection::MissingFailureEvidence)
    );
    let bad_kind = json!([{ "kind": "fix", "quote": FAILED_LINE }]);
    assert_eq!(
        validate_receipt(&raw(bad_kind, "failure"), &archive, &body, false),
        Err(ReceiptRejection::UnverifiableQuote)
    );
    let too_many = Value::Array(vec![json!({ "kind": "failure", "quote": FAILED_LINE }); 13]);
    assert_eq!(
        validate_receipt(&raw(too_many, "failure"), &archive, &body, false),
        Err(ReceiptRejection::SchemaMismatch)
    );
    let wrong_hash = raw(json!([]), "failure").replace(&archive.hash, &"0".repeat(64));
    assert_eq!(
        validate_receipt(&wrong_hash, &archive, &body, false),
        Err(ReceiptRejection::SchemaMismatch)
    );
}

#[test]
fn diagnostic_commands_match_only_at_command_boundaries() {
    for code in [
        "!pytest -q",
        r#"r = await bash("cargo test -p pa-core")"#,
        "await bash('npm run check')",
        "!make",
        "python3 -m pytest tests",
        "`tsc --noEmit`",
    ] {
        assert!(is_diagnostic_command(code), "{code}");
    }
    for code in [
        "print('hello')",
        "makefile = 1",
        "x.pytest_plugin()",
        "remake()",
    ] {
        assert!(!is_diagnostic_command(code), "{code}");
    }
    assert!(has_likely_secret("Authorization: Bearer abc"));
    assert!(!has_likely_secret("1 failed, 399 passed"));
}

#[test]
fn an_archive_with_different_bytes_is_an_integrity_failure() {
    let (dir, archive) = archive_for("payload\n");
    assert_eq!(archive_body(dir.path(), "payload\n").unwrap(), archive);
    std::fs::write(&archive.path, "tampered\n").unwrap();
    assert!(archive_body(dir.path(), "payload\n").is_err());
}
