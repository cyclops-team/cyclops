use super::*;

fn initialized() -> Vec<u8> {
    b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\"}}\n".to_vec()
}

#[test]
fn handshake_and_catalog_need_no_daemon_and_stdout_is_only_jsonrpc() {
    let mut input = initialized();
    input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n");
    let mut output = Vec::new();
    serve(&mut &input[..], &mut output, |_, _| {
        panic!("no daemon needed")
    })
    .unwrap();
    let responses: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-11-25");
    assert!(responses[0]["result"]["capabilities"]
        .get("experimental")
        .is_none());
    assert_eq!(responses[1]["result"]["tools"].as_array().unwrap().len(), 5);
    assert!(responses
        .iter()
        .all(|response| response["jsonrpc"] == "2.0"));
}

#[test]
fn channel_opt_in_starts_once_after_the_host_finishes_initialization() {
    let mut input = initialized();
    input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n");
    let mut output = Vec::new();
    let mut starts = 0;
    serve_configured(
        &mut &input[..],
        &mut output,
        |_, _| panic!("no tool call"),
        true,
        || {
            starts += 1;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(starts, 1);
    let response: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        response["result"]["capabilities"]["experimental"]["claude/channel"],
        json!({})
    );
}

#[test]
fn channel_authority_failure_does_not_emit_a_fake_notification() {
    let mut input = initialized();
    input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n");
    let mut output = Vec::new();
    assert!(serve_configured(
        &mut &input[..],
        &mut output,
        |_, _| panic!("no tool call"),
        true,
        || Err(ClientError::NotSent("not an agent".into()))
    )
    .is_err());
    let response: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(response["id"], 1);
    assert!(response.get("method").is_none());
}

#[test]
fn tool_translation_preserves_content_but_never_accepts_admin_or_sender_controls() {
    let params = json!({"to": ["reviewer"], "subject": "Review", "summary": "Please review.", "body": "quotes \" and\nnewlines", "client_key": "unique"});
    let (method, translated) = tools::prepare("cyclops_send", params.clone()).unwrap();
    assert_eq!(method, "msg.send");
    assert_eq!(translated, params);
    for forbidden in ["from", "sender", "expected_caller", "raw", "recipient_keys"] {
        let mut forged = params.clone();
        forged[forbidden] = json!("admin");
        assert!(
            tools::prepare("cyclops_send", forged).is_err(),
            "{forbidden}"
        );
    }
    for method in [
        "daemon.stop",
        "msg.read",
        "notification.withdraw",
        "headless.clear",
        "cyclops_admin",
    ] {
        assert!(tools::prepare(method, json!({})).is_err());
    }
}

#[test]
fn each_public_tool_maps_to_one_existing_domain_operation() {
    for (name, args, expected) in [
        (
            "cyclops_reply",
            json!({"message_id": "m-id", "summary": "Reply.", "body": "b"}),
            "msg.reply",
        ),
        ("cyclops_inbox", json!({}), "inbox.list"),
        (
            "cyclops_claim",
            json!({"message_id": "m-att_exact"}),
            "inbox.claim",
        ),
        (
            "cyclops_thread",
            json!({"id": "m-id", "body": true}),
            "msg.thread",
        ),
    ] {
        assert_eq!(tools::prepare(name, args).unwrap().0, expected);
    }
}

#[test]
fn summary_omission_leaves_the_existing_daemon_fallback_in_charge() {
    for (name, args) in [
        (
            "cyclops_send",
            json!({"to": ["reviewer"], "subject": "Review", "body": "private body"}),
        ),
        (
            "cyclops_reply",
            json!({"message_id": "m-id", "body": "private reply"}),
        ),
    ] {
        let (_, params) = tools::prepare(name, args).unwrap();
        assert!(params["summary"].is_null());
        let catalog = tools::catalog();
        let tool = catalog
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap();
        assert!(!tool["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("summary")));
    }
}

#[test]
fn a_tool_notification_or_uninitialized_call_cannot_mutate_the_daemon() {
    let mut state = false;
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "cyclops_inbox"}});
    let response = handle(request.clone(), &mut state, &mut |_, _| {
        panic!("not initialized")
    });
    assert_eq!(response.unwrap()["error"]["code"], -32002);
    let mut notification = request;
    notification.as_object_mut().unwrap().remove("id");
    assert!(handle(notification, &mut true, &mut |_, _| panic!(
        "notification must not call tools"
    ))
    .is_none());
}

#[test]
fn an_uncertain_write_is_reported_once_and_never_retried() {
    let mut calls = 0;
    let response = handle(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "cyclops_claim", "arguments": {"message_id": "m-id"}}}), &mut true, &mut |_, _| {
        calls += 1;
        Err(ClientError::Unknown("connection closed after write".into()))
    }).unwrap();
    assert_eq!(calls, 1);
    assert_eq!(response["result"]["isError"], true);
    let error: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(error["certainty"], "OutcomeUnknown");
}

#[test]
fn oversized_and_unterminated_input_never_reaches_the_daemon() {
    for input in [
        vec![b'x'; FrameContract::MAX_JSON_BYTES + 2],
        b"{}".to_vec(),
    ] {
        let mut output = Vec::new();
        assert!(serve(&mut &input[..], &mut output, |_, _| panic!("invalid frame")).is_err());
        assert!(output.is_empty());
    }
}

#[test]
fn an_oversized_response_preserves_uncertainty_and_has_a_bounded_frame() {
    let mut output = Vec::new();
    write_frame(
        &mut output,
        json!({"jsonrpc":"2.0","id":7,"result":"x".repeat(FrameContract::MAX_JSON_BYTES)}),
    )
    .unwrap();
    assert!(output.len() <= FrameContract::MAX_JSON_BYTES + 1);
    let response: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(response["id"], 7);
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("may already have completed"));
}

#[test]
fn malformed_frames_return_parse_errors_without_poisoning_the_next_request() {
    let input = b"not json\n{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"ping\"}\n";
    let mut output = Vec::new();
    serve(&mut &input[..], &mut output, |_, _| panic!("protocol only")).unwrap();
    let responses: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(responses[0]["error"]["code"], -32700);
    assert_eq!(responses[1]["id"], 8);
}
