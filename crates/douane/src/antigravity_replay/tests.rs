use super::*;
use serde_json::json;

fn scope() -> Scope {
    Scope {
        account: "synthetic-account".into(),
        model: "gemini-synthetic".into(),
        session: "synthetic-session".into(),
    }
}

fn request() -> Value {
    json!({"contents":[{"role":"user","parts":[{"text":"synthetic request"}]}]})
}

fn contents(request: &Value) -> Vec<Value> {
    request["contents"].as_array().unwrap().clone()
}

fn signed_call(id: &str, arg: i64) -> Value {
    json!({"functionCall":{"id":id,"name":"synthetic_tool","args":{"n":arg}},"thoughtSignature":"SYNTHETIC_SIGNATURE"})
}

fn unsigned_call(id: &str, arg: i64) -> Value {
    json!({"functionCall":{"id":id,"name":"synthetic_tool","args":{"n":arg}}})
}

fn store(cache: &mut ReplayCache, scope: Scope, request: &Value, parts: Value) -> Value {
    let mut recorder = Recorder::new(scope, contents(request));
    let mut event = json!({"response":{"candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}]}});
    recorder.observe(&mut event);
    let turn = recorder
        .finish()
        .expect("terminal native output must yield a turn");
    assert!(cache.record(turn), "bounded native turn must be recorded");
    event["response"]["candidates"][0]["content"]["parts"].clone()
}

fn append(request: &mut Value, role: &str, parts: Value) {
    request["contents"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":role,"parts":parts}));
}

#[test]
fn two_tool_cycles_restore_both_original_signed_turns() {
    let mut cache = ReplayCache::default();
    let initial = request();
    let first = json!([{"thought":true,"text":"hidden first","thoughtSignature":"SYNTHETIC_HIDDEN_1"}, signed_call("call_first",1)]);
    store(&mut cache, scope(), &initial, first.clone());
    let mut second_request = initial.clone();
    append(
        &mut second_request,
        "model",
        json!([unsigned_call("call_first", 1)]),
    );
    append(
        &mut second_request,
        "user",
        json!([{"functionResponse":{"id":"call_first","name":"synthetic_tool","response":{"result":1}}}]),
    );
    cache.restore(&scope(), &mut second_request).unwrap();
    assert_eq!(second_request["contents"][1]["parts"], first);
    let second = json!([signed_call("call_second",2),{"text":"after tool"}]);
    store(&mut cache, scope(), &second_request, second.clone());
    let mut third_request = initial;
    append(
        &mut third_request,
        "model",
        json!([unsigned_call("call_first", 1)]),
    );
    append(
        &mut third_request,
        "user",
        json!([{"functionResponse":{"id":"call_first","name":"synthetic_tool","response":{"result":1}}}]),
    );
    append(
        &mut third_request,
        "model",
        json!([unsigned_call("call_second",2),{"text":"after tool"}]),
    );
    append(
        &mut third_request,
        "user",
        json!([{"functionResponse":{"id":"call_second","name":"synthetic_tool","response":{"result":2}}}]),
    );
    cache.restore(&scope(), &mut third_request).unwrap();
    assert_eq!(third_request["contents"][1]["parts"], first);
    assert_eq!(third_request["contents"][3]["parts"], second);
}

#[test]
fn streamed_interleaved_thoughts_and_detached_signatures_keep_order() {
    let initial = request();
    let mut recorder = Recorder::new(scope(), contents(&initial));
    let mut first = json!({"candidates":[{"content":{"parts":[
        {"thought":true,"text":"hidden","thoughtSignature":"SYNTHETIC_HIDDEN"},
        {"text":"visible "},
        {"functionCall":{"name":"synthetic_tool","args":{"n":1}},"thoughtSignature":"SYNTHETIC_ATTACHED"},
        {"thought_signature":"SYNTHETIC_DETACHED"}
    ]}}]});
    let mut last = json!({"response":{"candidates":[{"content":{"parts":[
        {"thought":true,"text":"another thought"},
        {"functionCall":{"name":"synthetic_tool","args":{"n":2}}}
    ]},"finishReason":"STOP"}]}});
    recorder.observe(&mut first);
    recorder.observe(&mut last);
    let id1 = first["candidates"][0]["content"]["parts"][2]["functionCall"]["id"]
        .as_str()
        .expect("ID injected before conversion");
    let id2 = last["response"]["candidates"][0]["content"]["parts"][1]["functionCall"]["id"]
        .as_str()
        .unwrap();
    assert_ne!(id1, id2);
    let mut expected = first["candidates"][0]["content"]["parts"]
        .as_array()
        .unwrap()
        .clone();
    expected.extend(
        last["response"]["candidates"][0]["content"]["parts"]
            .as_array()
            .unwrap()
            .clone(),
    );
    let mut cache = ReplayCache::default();
    assert!(cache.record(recorder.finish().unwrap()));
    let mut replay = initial;
    append(
        &mut replay,
        "model",
        json!([{"text":"visible "},unsigned_call(id1,1),unsigned_call(id2,2)]),
    );
    cache.restore(&scope(), &mut replay).unwrap();
    assert_eq!(replay["contents"][1]["parts"], json!(expected));
}

#[test]
fn duplicate_parallel_calls_get_distinct_ids_and_replay_by_id() {
    let mut cache = ReplayCache::default();
    let initial = request();
    let native = store(
        &mut cache,
        scope(),
        &initial,
        json!([
            {"functionCall":{"name":"same","args":{"x":1}},"thoughtSignature":"SYNTHETIC_1"},
            {"functionCall":{"name":"same","args":{"x":1}},"thoughtSignature":"SYNTHETIC_2"},
            {"functionCall":{"id":"upstream_id","name":"same","args":{"x":1}},"thoughtSignature":"SYNTHETIC_3"}
        ]),
    );
    assert_ne!(
        native[0]["functionCall"]["id"],
        native[1]["functionCall"]["id"]
    );
    assert_eq!(native[2]["functionCall"]["id"], "upstream_id");
    let mut replay = initial;
    append(
        &mut replay,
        "model",
        json!([
            {"functionCall":native[0]["functionCall"]},
            {"functionCall":native[1]["functionCall"]},
            {"functionCall":native[2]["functionCall"]}
        ]),
    );
    cache.restore(&scope(), &mut replay).unwrap();
    assert_eq!(replay["contents"][1]["parts"], native);
}

#[test]
fn merging_deltas_preserves_signed_text_boundaries_and_thought_flags() {
    let initial = request();
    let mut recorder = Recorder::new(scope(), contents(&initial));
    let mut event = json!({"candidates":[{"content":{"parts":[
        {"thought":true,"text":"hidden "},{"thought":true,"text":"thought"},
        {"text":"hello "},{"text":"world"},
        {"text":"signed","thoughtSignature":"SYNTHETIC_TEXT"},
        {"text":"after"},{"thought":true,"text":"final hidden"},
        {"thought_signature":"SYNTHETIC_DETACHED"}
    ]},"finishReason":"STOP"}]});
    recorder.observe(&mut event);
    let mut cache = ReplayCache::default();
    assert!(cache.record(recorder.finish().unwrap()));
    let mut replay = initial;
    append(
        &mut replay,
        "model",
        json!([{"text":"hello worldsignedafter"}]),
    );
    cache.restore(&scope(), &mut replay).unwrap();
    assert_eq!(
        replay["contents"][1]["parts"],
        json!([
            {"thought":true,"text":"hidden thought"},
            {"text":"hello world"},
            {"text":"signed","thoughtSignature":"SYNTHETIC_TEXT"},
            {"text":"after"},{"thought":true,"text":"final hidden"},
            {"thought_signature":"SYNTHETIC_DETACHED"}
        ])
    );
}

#[test]
fn altered_call_arguments_name_id_or_order_never_inherit_signatures() {
    let initial = request();
    let mut cache = ReplayCache::default();
    store(
        &mut cache,
        scope(),
        &initial,
        json!([signed_call("known_1", 1), signed_call("known_2", 2)]),
    );
    for changed in [
        json!([unsigned_call("known_1", 99), unsigned_call("known_2", 2)]),
        json!([{"functionCall":{"id":"known_1","name":"wrong","args":{"n":1}}},unsigned_call("known_2",2)]),
        json!([unsigned_call("changed_id", 1), unsigned_call("known_2", 2)]),
        json!([unsigned_call("known_2", 2), unsigned_call("known_1", 1)]),
    ] {
        let mut replay = initial.clone();
        append(&mut replay, "model", changed);
        let original = replay.clone();
        assert!(cache.restore(&scope(), &mut replay).is_err());
        assert_eq!(replay, original, "failed restoration must be atomic");
    }
}

#[test]
fn preceding_user_context_and_tool_results_are_part_of_the_identity() {
    let initial = request();
    let mut cache = ReplayCache::default();
    store(
        &mut cache,
        scope(),
        &initial,
        json!([signed_call("known", 1)]),
    );
    let mut replay = initial.clone();
    replay["contents"][0]["parts"][0]["text"] = json!("changed context");
    append(&mut replay, "model", json!([unsigned_call("known", 1)]));
    assert!(cache.restore(&scope(), &mut replay).is_err());
    let mut second_context = initial;
    append(
        &mut second_context,
        "model",
        json!([unsigned_call("known", 1)]),
    );
    append(
        &mut second_context,
        "user",
        json!([{"functionResponse":{"id":"known","name":"synthetic_tool","response":{"result":"original"}}}]),
    );
    cache.restore(&scope(), &mut second_context).unwrap();
    store(
        &mut cache,
        scope(),
        &second_context,
        json!([signed_call("second_known", 2)]),
    );
    second_context["contents"][2]["parts"][0]["functionResponse"]["response"]["result"] =
        json!("changed");
    append(
        &mut second_context,
        "model",
        json!([unsigned_call("second_known", 2)]),
    );
    let original = second_context.clone();
    assert!(cache.restore(&scope(), &mut second_context).is_err());
    assert_eq!(second_context, original);
}

#[test]
fn credential_model_and_session_scopes_cannot_exchange_signed_turns() {
    let initial = request();
    let mut cache = ReplayCache::default();
    store(
        &mut cache,
        scope(),
        &initial,
        json!([signed_call("scoped", 1)]),
    );
    for other in [
        Scope {
            account: "other".into(),
            ..scope()
        },
        Scope {
            model: "other".into(),
            ..scope()
        },
        Scope {
            session: "other".into(),
            ..scope()
        },
    ] {
        let mut replay = initial.clone();
        append(&mut replay, "model", json!([unsigned_call("scoped", 1)]));
        let original = replay.clone();
        assert!(cache.restore(&other, &mut replay).is_err());
        assert_eq!(replay, original);
    }
}

#[test]
fn signed_plain_text_requires_matching_context_and_scope() {
    let initial = request();
    let mut cache = ReplayCache::default();
    let native = json!([{"thought":true,"text":"private reasoning"},{"text":"hello ","thoughtSignature":"SYNTHETIC_TEXT"},{"text":"world"}]);
    store(&mut cache, scope(), &initial, native.clone());
    let mut replay = initial.clone();
    append(
        &mut replay,
        "model",
        json!([{"text":"hello"},{"text":" world"}]),
    );
    cache.restore(&scope(), &mut replay).unwrap();
    assert_eq!(replay["contents"][1]["parts"], native);
    let mut wrong_context = initial.clone();
    wrong_context["contents"][0]["parts"][0]["text"] = json!("other question");
    append(&mut wrong_context, "model", json!([{"text":"hello world"}]));
    let original = wrong_context.clone();
    assert!(cache.restore(&scope(), &mut wrong_context).is_err());
    assert_eq!(wrong_context, original);
    let mut wrong_scope = initial;
    append(&mut wrong_scope, "model", json!([{"text":"hello world"}]));
    let original = wrong_scope.clone();
    cache
        .restore(
            &Scope {
                account: "other".into(),
                ..scope()
            },
            &mut wrong_scope,
        )
        .unwrap_err();
    assert_eq!(wrong_scope, original);
}

#[test]
fn interrupted_and_error_streams_never_finalize_a_turn() {
    let mut recorder = Recorder::new(scope(), contents(&request()));
    let mut event = json!({"candidates":[{"content":{"parts":[signed_call("partial",1)]}}]});
    recorder.observe(&mut event);
    assert!(recorder.finish().is_none(), "missing terminal finishReason");
    let mut recorder = Recorder::new(scope(), contents(&request()));
    event["candidates"][0]["finishReason"] = json!("STOP");
    recorder.observe(&mut event);
    recorder.abort();
    assert!(
        recorder.finish().is_none(),
        "transport disconnect after terminal"
    );
    let mut recorder = Recorder::new(scope(), contents(&request()));
    recorder.observe(&mut event);
    recorder.observe(&mut json!({"error":{"message":"synthetic stream failure"}}));
    assert!(recorder.finish().is_none());
}

#[test]
fn oversized_native_output_is_discarded_before_recording() {
    let mut recorder = Recorder::new(scope(), contents(&request()));
    let mut event = json!({"candidates":[{"content":{"parts":[{"text":"x".repeat(256*1024+1),"thoughtSignature":"SYNTHETIC_LARGE"}]},"finishReason":"STOP"}]});
    recorder.observe(&mut event);
    assert!(recorder.finish().is_none());
    let mut recorder = Recorder::new(scope(), contents(&request()));
    recorder.observe(&mut json!({"candidates":[{"content":{"parts":[signed_call("too_large_for_cache",1)]},"finishReason":"STOP"}]}));
    let mut cache = ReplayCache::with_limits(1024, 8, Duration::from_secs(60));
    assert!(!cache.record(recorder.finish().unwrap()));
}

fn turn_for(scope: Scope, request: &Value, text: &str) -> RecordedTurn {
    let mut recorder = Recorder::new(scope, contents(request));
    recorder.observe(&mut json!({"candidates":[{"content":{"parts":[{"text":text,"thoughtSignature":"SYNTHETIC"}]},"finishReason":"STOP"}]}));
    recorder.finish().unwrap()
}

fn text_history(text: &str) -> Value {
    let mut history = request();
    append(&mut history, "model", json!([{"text":text}]));
    history
}

#[test]
fn byte_budget_evicts_whole_least_recently_used_conversations() {
    let now = Instant::now();
    let scopes = [
        scope(),
        Scope {
            session: "second-session".into(),
            ..scope()
        },
        Scope {
            session: "third-session".into(),
            ..scope()
        },
    ];
    let turns = ["first", "second"].map(|text| turn_for(scopes[0].clone(), &request(), text));
    let budget =
        turns.iter().map(|turn| turn.bytes).sum::<usize>() + conversation_bytes(&scopes[0]);
    let mut cache = ReplayCache::with_limits(budget, 4096, Duration::from_secs(60));
    for turn in turns {
        assert!(cache.record_at(turn, now));
    }
    assert!(cache.record_at(turn_for(scopes[1].clone(), &request(), "new"), now));
    assert!(
        !cache.conversations.contains_key(&scopes[0]),
        "no old tail may survive eviction"
    );
    assert!(
        cache
            .restore_at(&scopes[0], &mut text_history("first"), now)
            .is_err()
    );
    assert!(
        cache
            .restore_at(&scopes[0], &mut text_history("second"), now)
            .is_err()
    );
    assert!(cache.bytes <= budget);

    // Give three one-turn conversations enough room for exactly two. A valid
    // restore makes the older conversation newer than the second one.
    let budget = scopes
        .iter()
        .map(|scope| turn_for(scope.clone(), &request(), "same").bytes + conversation_bytes(scope))
        .max()
        .unwrap()
        * 2;
    let mut cache = ReplayCache::with_limits(budget, 4096, Duration::from_secs(60));
    assert!(cache.record_at(turn_for(scopes[0].clone(), &request(), "same"), now));
    assert!(cache.record_at(
        turn_for(scopes[1].clone(), &request(), "same"),
        now + Duration::from_secs(1)
    ));
    cache
        .restore_at(
            &scopes[0],
            &mut text_history("same"),
            now + Duration::from_secs(2),
        )
        .unwrap();
    assert!(cache.record_at(
        turn_for(scopes[2].clone(), &request(), "same"),
        now + Duration::from_secs(3)
    ));
    assert!(cache.conversations.contains_key(&scopes[0]));
    assert!(!cache.conversations.contains_key(&scopes[1]));
}

#[test]
fn extending_a_full_conversation_is_refused_without_losing_its_prefix() {
    let first = turn_for(scope(), &request(), "first");
    let budget = first.bytes + conversation_bytes(&scope());
    let mut cache = ReplayCache::with_limits(budget, 4096, Duration::from_secs(60));
    assert!(cache.record(first));
    let before = cache.bytes;
    assert!(!cache.record(turn_for(scope(), &text_history("first"), "second")));
    assert_eq!(cache.bytes, before);
    assert_eq!(cache.conversations[&scope()].turns.len(), 1);
    cache.restore(&scope(), &mut text_history("first")).unwrap();
}

#[test]
fn active_calls_pin_their_whole_conversation_and_release_on_cancellation() {
    let first = turn_for(scope(), &request(), "first");
    let budget = first.bytes + conversation_bytes(&scope());
    let mut cache = ReplayCache::with_limits(budget, 4096, Duration::from_secs(60));
    assert!(cache.record(first));
    cache.begin(&scope(), &mut text_history("first")).unwrap();
    cache.begin(&scope(), &mut text_history("first")).unwrap();
    let other = Scope {
        session: "another-session".into(),
        ..scope()
    };
    assert!(!cache.record(turn_for(other.clone(), &request(), "other")));
    cache.release(&scope());
    assert!(!cache.record(turn_for(other.clone(), &request(), "other")));
    cache.release(&scope());
    assert!(cache.record(turn_for(other.clone(), &request(), "other")));
    assert!(!cache.conversations.contains_key(&scope()));
    let mut empty = ReplayCache::default();
    empty.begin(&scope(), &mut request()).unwrap();
    empty.release(&scope());
    assert_eq!(empty.bytes, 0);
    assert!(empty.conversations.is_empty());
}

#[test]
fn ttl_slides_only_after_a_fully_valid_restore() {
    let now = Instant::now();
    let mut cache = ReplayCache::with_limits(65536, 4096, Duration::from_secs(10));
    assert!(cache.record_at(turn_for(scope(), &request(), "first"), now));
    cache
        .restore_at(
            &scope(),
            &mut text_history("first"),
            now + Duration::from_secs(9),
        )
        .unwrap();
    cache
        .restore_at(
            &scope(),
            &mut text_history("first"),
            now + Duration::from_secs(18),
        )
        .unwrap();
    assert!(cache.record_at(
        turn_for(scope(), &request(), "second"),
        now + Duration::from_secs(19)
    ));
    let other = Scope {
        session: "changed-session".into(),
        ..scope()
    };
    assert!(
        cache
            .restore_at(
                &other,
                &mut text_history("first"),
                now + Duration::from_secs(26)
            )
            .is_err()
    );
    let mut partially_valid = text_history("first");
    append(&mut partially_valid, "model", json!([{"text":"altered"}]));
    assert!(
        cache
            .restore_at(
                &scope(),
                &mut partially_valid,
                now + Duration::from_secs(27)
            )
            .is_err()
    );
    assert!(
        cache
            .restore_at(
                &scope(),
                &mut text_history("first"),
                now + Duration::from_secs(28)
            )
            .is_err(),
        "failed restore and record must not renew TTL"
    );
    assert_eq!(cache.bytes, 0);
}

#[test]
fn expired_turns_are_not_replayed() {
    let initial = request();
    let mut cache = ReplayCache::with_limits(8192, 4096, Duration::ZERO);
    store(
        &mut cache,
        scope(),
        &initial,
        json!([signed_call("expired", 1)]),
    );
    let mut replay = initial;
    append(&mut replay, "model", json!([unsigned_call("expired", 1)]));
    let original = replay.clone();
    assert!(cache.restore(&scope(), &mut replay).is_err());
    assert_eq!(replay, original);
}

#[test]
fn missing_sessions_assign_ids_but_never_store_or_restore_history() {
    let anonymous = Scope {
        session: String::new(),
        ..scope()
    };
    let initial = request();
    let mut recorder = Recorder::new(anonymous.clone(), contents(&initial));
    let mut event = json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"tool","args":{}}}]},"finishReason":"STOP"}]});
    recorder.observe(&mut event);
    assert!(
        event["candidates"][0]["content"]["parts"][0]["functionCall"]["id"]
            .as_str()
            .is_some()
    );
    assert!(recorder.finish().is_none());
    let mut cache = ReplayCache::default();
    let mut replay = initial;
    cache.restore(&anonymous, &mut replay).unwrap();
    append(&mut replay, "model", json!([unsigned_call("missing", 1)]));
    assert!(
        cache
            .restore(&anonymous, &mut replay)
            .unwrap_err()
            .contains("stable session")
    );
}

#[test]
fn existing_duplicate_upstream_ids_are_not_rewritten_or_recorded() {
    let mut recorder = Recorder::new(scope(), contents(&request()));
    let mut event = json!({"candidates":[{"content":{"parts":[signed_call("duplicate",1),signed_call("duplicate",2)]},"finishReason":"STOP"}]});
    recorder.observe(&mut event);
    assert_eq!(
        event["candidates"][0]["content"]["parts"][0]["functionCall"]["id"],
        "duplicate"
    );
    assert_eq!(
        event["candidates"][0]["content"]["parts"][1]["functionCall"]["id"],
        "duplicate"
    );
    assert!(recorder.finish().is_none());
}

#[test]
fn unknown_unsigned_calls_fail_locally_and_native_signed_calls_pass() {
    let mut cache = ReplayCache::default();
    let mut unsigned = request();
    append(&mut unsigned, "model", json!([unsigned_call("unknown", 1)]));
    let original = unsigned.clone();
    assert!(cache.restore(&scope(), &mut unsigned).is_err());
    assert_eq!(unsigned, original);
    let mut native = request();
    append(&mut native, "model", json!([signed_call("unknown", 1)]));
    let original = native.clone();
    cache.restore(&scope(), &mut native).unwrap();
    assert_eq!(native, original);
}

#[test]
fn wrapped_requests_and_argument_key_order_restore_the_same_native_turn() {
    let initial = request();
    let mut cache = ReplayCache::default();
    let native = json!([{"functionCall":{"id":"ordered_args","name":"tool","args":{"a":1,"b":{"x":2,"y":3}}},"thoughtSignature":"SYNTHETIC_ORDERED"}]);
    store(&mut cache, scope(), &initial, native.clone());
    let mut replay = initial;
    append(&mut replay,"model",serde_json::from_str(r#"[{"functionCall":{"args":{"b":{"y":3,"x":2},"a":1},"name":"tool","id":"ordered_args"}}]"#).unwrap());
    let mut wrapped = json!({"request":replay});
    cache.restore(&scope(), &mut wrapped).unwrap();
    assert_eq!(wrapped["request"]["contents"][1]["parts"], native);
}

#[test]
fn wire_text_first_projection_restores_interleaved_native_parts_exactly() {
    let initial = request();
    let native = json!([
        {"text":"before ","thoughtSignature":"SYNTHETIC_BEFORE"},
        signed_call("mixed_first",1),
        {"thought":true,"text":"hidden middle","thought_signature":"SYNTHETIC_HIDDEN"},
        {"text":"middle "},
        signed_call("mixed_second",2),
        {"thought_signature":"SYNTHETIC_DETACHED"},
        {"text":"after"}
    ]);
    let mut cache = ReplayCache::default();
    store(&mut cache, scope(), &initial, native.clone());
    let mut replay = initial.clone();
    append(
        &mut replay,
        "model",
        json!([
            {"text":"before middle after"},
            unsigned_call("mixed_first",1),
            unsigned_call("mixed_second",2)
        ]),
    );
    cache.restore(&scope(), &mut replay).unwrap();
    assert_eq!(replay["contents"][1]["parts"], native);
    let mut altered = initial;
    append(
        &mut altered,
        "model",
        json!([
            {"text":"before middle after"},
            unsigned_call("mixed_second",2),
            unsigned_call("mixed_first",1)
        ]),
    );
    assert!(
        cache.restore(&scope(), &mut altered).is_err(),
        "wire projection must retain call ordering"
    );
}

#[test]
fn model_wire_projection_does_not_normalize_user_image_and_text_order() {
    let initial = json!({"contents":[{"role":"user","parts":[
        {"text":"before "},
        {"inlineData":{"mimeType":"image/png","data":"U1lOVEhFVElD"}},
        {"text":"after"}
    ]}]});
    let native = json!([signed_call("image_context",1),{"text":"visible"}]);
    let mut cache = ReplayCache::default();
    store(&mut cache, scope(), &initial, native.clone());
    let mut replay = initial.clone();
    append(
        &mut replay,
        "model",
        json!([{"text":"visible"},unsigned_call("image_context",1)]),
    );
    cache.restore(&scope(), &mut replay).unwrap();
    assert_eq!(replay["contents"][1]["parts"], native);
    let mut changed = initial;
    changed["contents"][0]["parts"] = json!([
        {"text":"before after"},
        {"inlineData":{"mimeType":"image/png","data":"U1lOVEhFVElD"}}
    ]);
    append(
        &mut changed,
        "model",
        json!([{"text":"visible"},unsigned_call("image_context",1)]),
    );
    let original = changed.clone();
    assert!(cache.restore(&scope(), &mut changed).is_err());
    assert_eq!(changed, original);
}

#[test]
fn long_concurrent_conversations_keep_every_signed_turn_below_the_byte_budget() {
    let mut cache = ReplayCache::default();
    let scopes = [
        scope(),
        Scope {
            session: "second-session".into(),
            ..scope()
        },
    ];
    let mut histories = [request(), request()];
    for turn in 0..160 {
        for (index, history) in histories.iter_mut().enumerate() {
            let mut outgoing = history.clone();
            cache
                .restore(&scopes[index], &mut outgoing)
                .expect("active history must remain replayable beyond 129 total turns");
            let id = format!("long_{index}_{turn}");
            store(
                &mut cache,
                scopes[index].clone(),
                &outgoing,
                json!([signed_call(&id, turn)]),
            );
            append(history, "model", json!([unsigned_call(&id, turn)]));
            append(
                history,
                "user",
                json!([{"functionResponse":{"id":id,"name":"synthetic_tool","response":{"result":"ok"}}}]),
            );
        }
    }
    for (index, history) in histories.iter_mut().enumerate() {
        cache.restore(&scopes[index], history).unwrap();
        assert!(cache.bytes <= 8 * 1024 * 1024);
    }
}

#[test]
fn missing_or_expired_plain_text_history_is_refused_instead_of_sent_unsigned() {
    let initial = request();
    let mut cache = ReplayCache::with_limits(8192, 4096, Duration::ZERO);
    store(
        &mut cache,
        scope(),
        &initial,
        json!([{"thought":true,"text":"private"},{"text":"first answer","thoughtSignature":"SYNTHETIC_TEXT"}]),
    );
    let mut history = initial;
    append(&mut history, "model", json!([{"text":"first answer"}]));
    append(
        &mut history,
        "user",
        json!([{"text":"Please return valid JSON."}]),
    );
    let original = history.clone();
    assert!(cache.restore(&scope(), &mut history).is_err());
    assert_eq!(history, original);
    assert!(
        ReplayCache::default()
            .restore(&scope(), &mut history)
            .is_err(),
        "restart must fail explicitly"
    );
}
