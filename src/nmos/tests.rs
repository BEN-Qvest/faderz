use super::*;

/// A `/bulk` cancel is taken into the state only where samplez-nmos
/// answered it 2xx and no later item of the request names its port,
/// whose schedule the cancel would otherwise remove.
#[test]
fn a_bulk_cancel_is_taken_per_item() {
    let asked = [
        Some(("a", 0, true)),
        Some(("b", 1, true)),
        Some(("c", 2, true)),
        Some(("c", 2, false)),
        None,
        Some(("d", 3, false)),
    ];
    let answered = [
        serde_json::json!({ "id": "a", "code": 400 }),
        serde_json::json!({ "id": "b", "code": 200 }),
        serde_json::json!({ "id": "c", "code": 200 }),
        serde_json::json!({ "id": "c", "code": 202 }),
        serde_json::json!({ "id": "x", "code": 404 }),
        serde_json::json!({ "id": "d", "code": 202 }),
    ];
    assert_eq!(bulk_cancels(&asked, &answered), vec![(1, 1)]);
    // A response out of step with the request cancels nothing.
    let other = [serde_json::json!({ "id": "a", "code": 200 })];
    assert_eq!(bulk_cancels(&asked[1..2], &other), vec![]);
}

/// A staging request's scheduled activation is kept with the mode
/// and requested time it was asked with; an immediate one, a cancel
/// and a request without an activation keep nothing.
#[test]
fn a_scheduled_request_keeps_its_mode_and_requested_time() {
    let asked = |mode: serde_json::Value, requested: serde_json::Value| {
        scheduled_request(&serde_json::json!({
            "activation": { "mode": mode, "requested_time": requested }
        }))
    };
    assert_eq!(
        asked("activate_scheduled_relative".into(), "0:200000000".into()),
        Some(Activation {
            mode: "activate_scheduled_relative".into(),
            requested: Some("0:200000000".into()),
            time: String::new(),
        })
    );
    assert_eq!(
        asked("activate_scheduled_absolute".into(), "1700000000:0".into()).map(|a| a.mode),
        Some("activate_scheduled_absolute".into())
    );
    assert_eq!(
        asked("activate_immediate".into(), serde_json::Value::Null),
        None
    );
    assert_eq!(
        asked(serde_json::Value::Null, serde_json::Value::Null),
        None
    );
    assert_eq!(
        scheduled_request(&serde_json::json!({ "master_enable": true })),
        None
    );
}

#[test]
fn a_flow_definition_reads_as_channels_rate_and_format() {
    let def = r#"{"format":"urn:x-nmos:format:audio","media_type":"audio/float32",
            "sample_rate":{"numerator":48000},"channel_count":2,"bit_depth":32}"#;
    let seen = flow_seen(def);
    assert_eq!(
        seen,
        Some(FlowSeen::Present {
            channels: 2,
            rate: 48_000,
            float32: true
        })
    );
    let other = def.replace("audio/float32", "audio/L24");
    assert!(matches!(
        flow_seen(&other),
        Some(FlowSeen::Present { float32: false, .. })
    ));
    assert_eq!(flow_seen(r#"{"format":"urn:x-nmos:format:video"}"#), None);
}

fn mxl(domains: serde_json::Value) -> Mxl {
    serde_json::from_value(serde_json::json!({
        "domains": domains, "inputs": {}, "outputs": {}
    }))
    .unwrap()
}

#[test]
fn a_domain_id_comes_from_its_domain_def_else_the_configuration() {
    let dir = std::env::temp_dir().join(format!("faderz-nmos-ids-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let a = "6f1f1d1e-56f6-4c4c-8d32-9e7f4a3b2c10";
    let b = "6f1f1d1e-56f6-4c4c-8d32-9e7f4a3b2c11";
    let d = |id: Option<&str>| serde_json::json!([{ "name": "d", "path": dir, "id": id }]);
    // Neither: refused, no id is guessed.
    assert!(domain_ids(&mxl(d(None))).unwrap_err().contains("required"));
    assert_eq!(domain_ids(&mxl(d(Some(a)))).unwrap()["d"].to_string(), a);
    assert!(domain_ids(&mxl(d(Some(&a.to_uppercase())))).is_err());
    std::fs::write(
        dir.join(DOMAIN_DEF),
        format!(r#"{{"id":"{a}","label":"x"}}"#),
    )
    .unwrap();
    assert_eq!(domain_ids(&mxl(d(None))).unwrap()["d"].to_string(), a);
    assert_eq!(domain_ids(&mxl(d(Some(a)))).unwrap()["d"].to_string(), a);
    assert!(
        domain_ids(&mxl(d(Some(b))))
            .unwrap_err()
            .contains("differs")
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_forwarded_answer_is_passed_on_whole() {
    let plain = b"HTTP/1.1 423 Locked\r\ncontent-type: application/json\r\n\
        access-control-allow-origin: *\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
    let r = parse_response(plain).unwrap();
    assert_eq!(r.status(), StatusCode::LOCKED);
    assert_eq!(r.headers()["access-control-allow-origin"], "*");
    assert!(r.headers().get("connection").is_none());
    let chunked = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n\
        3\r\n[1,\r\n2;x=y\r\n2]\r\n0\r\n\r\n";
    assert_eq!(
        dechunk(b"3\r\n[1,\r\n2\r\n2]\r\n0\r\n\r\n").unwrap(),
        b"[1,2]"
    );
    assert_eq!(parse_response(chunked).unwrap().status(), StatusCode::OK);
    assert!(dechunk(b"5\r\n[1").is_none());
    assert!(parse_response(b"garbage").is_err());
}

struct Nothing;

impl Connections for Nothing {
    fn resolve(&self, _: Port, _: Uuid) -> BoxFuture<'_, Result<FlowSeen, Reject>> {
        Box::pin(async { Ok(FlowSeen::Absent) })
    }
    fn lead(&self) -> Duration {
        Duration::ZERO
    }
    fn apply(&self, _: Port, _: Active, _: Option<Tai>, _: u64) -> BoxFuture<'_, Result<(), Fail>> {
        Box::pin(async { Ok(()) })
    }
}

/// The ids staging requests are mapped to ports by are the ones the
/// Node API serves.
#[tokio::test]
async fn port_ids_are_the_node_apis() {
    use tower::ServiceExt;
    let domain = samplez_nmos::uuid::uuid!("6f1f1d1e-56f6-4c4c-8d32-9e7f4a3b2c10");
    let flow = samplez_nmos::uuid::uuid!("0d7f6b8e-4a2c-4e8f-9b1a-2c3d4e5f6a01");
    let model = NodeModel {
        label: "x".into(),
        description: "x".into(),
        senders: vec![SenderDesc {
            key: "out-pgm".into(),
            label: "PGM".into(),
            channels: vec![ChannelDesc {
                label: "L".into(),
                symbol: Some("L".into()),
            }],
            group: "out-pgm".into(),
            domain,
            flow,
        }],
        receivers: vec![ReceiverDesc {
            key: "ch1".into(),
            label: "Mic".into(),
            channels: ChannelCount::AtLeast(1),
            group: "ch1".into(),
            domain,
            flow: None,
        }],
    };
    let cfg = NmosConfig {
        seed: "faderz".into(),
        advertise_host: "127.0.0.1".into(),
        advertise_port: 8081,
        asset: Asset {
            manufacturer: "m".into(),
            product: "p".into(),
            instance_id: "i".into(),
            functions: vec!["Mixer".into()],
        },
        heartbeat: Duration::from_secs(5),
        registries: Vec::new(),
        dns_sd_domain: None,
        continue_held: false,
    };
    let nmos = samplez_nmos::Nmos::new(
        model,
        cfg,
        Arc::new(Nothing),
        &samplez_nmos::NoStore,
        SystemTai,
    )
    .unwrap();
    let get = |path: &'static str| {
        let router = nmos.router();
        async move {
            let req = Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap();
            let body = router.oneshot(req).await.unwrap().into_body();
            let bytes = axum::body::to_bytes(body, MAX_BODY).await.unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()[0]["id"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    let ports = [
        Model::Sender {
            output: "out-pgm".into(),
            domain,
            flow,
        },
        Model::Receiver {
            channel: "ch1".into(),
            domain,
        },
    ];
    let ids = port_ids("faderz", &ports);
    assert_eq!(ids[&(true, get("/x-nmos/node/v1.3/senders/").await)], 0);
    assert_eq!(ids[&(false, get("/x-nmos/node/v1.3/receivers/").await)], 1);
}

/// samplez-nmos's cancel, the port's current connection for the
/// scheduled time, is never taken as a schedule of its own, also
/// when the state's scheduled activation was cancelled already; and
/// another connection for the same time is a schedule.
#[test]
fn a_cancel_is_read_by_its_connection() {
    let flow = |n: u8| Uuid::parse_str(&format!("0d7f6b8e-4a2c-4e8f-9b1a-2c3d4e5f6a0{n}")).ok();
    let on = |n| Active {
        master_enable: true,
        peer: None,
        domain: None,
        flow: flow(n),
    };
    assert_eq!(
        scheduling(&on(1), &on(1), Some(5), 5, false),
        Scheduling::Cancel
    );
    assert_eq!(
        scheduling(&on(1), &on(1), None, 5, false),
        Scheduling::Nothing
    );
    assert_eq!(
        scheduling(&on(1), &on(1), Some(6), 5, false),
        Scheduling::Nothing
    );
    assert_eq!(
        scheduling(&on(2), &on(1), Some(5), 5, false),
        Scheduling::Take
    );
    assert_eq!(scheduling(&on(2), &on(1), None, 5, false), Scheduling::Take);
    // A cancel finds a schedule pending only before its time.
    let s = |at| Scheduled {
        id: "s".into(),
        port: PortRef::Sender("out-pgm".into()),
        active: connection(&on(1), None),
        at,
    };
    assert!(pending(Some(&s(5)), 4));
    assert!(!pending(Some(&s(5)), 5), "due: counts as done");
    assert!(!pending(None, 4));
    // A scheduled request staged here for the connection as it is, a
    // controller moving the port's activation alone, is taken.
    assert_eq!(scheduling(&on(1), &on(1), None, 5, true), Scheduling::Take);
}

#[test]
fn staging_paths_and_cancels_are_read() {
    assert_eq!(
        port_of("/x-nmos/connection/v1.2/single/receivers/abc/staged"),
        Some(("receivers", "abc"))
    );
    assert_eq!(port_of("/x-nmos/connection/v1.2/bulk/receivers"), None);
    assert!(cancels(
        &serde_json::json!({ "activation": { "mode": null } })
    ));
    assert!(!cancels(
        &serde_json::json!({ "activation": { "mode": "activate_immediate" } })
    ));
    assert!(!cancels(&serde_json::json!({ "master_enable": false })));
    let one = samplez_nmos::RESOLVE_TIMEOUT + FORWARD_MARGIN;
    assert_eq!(forward_timeout(b"{}"), one);
    assert_eq!(
        forward_timeout(b"[{},{},{}]"),
        samplez_nmos::RESOLVE_TIMEOUT * 3 + FORWARD_MARGIN
    );
}

fn parts() -> axum::http::request::Parts {
    let req = Request::builder()
        .method("PATCH")
        .uri("/x-nmos/connection/v1.2/single/receivers/x/staged")
        .body(())
        .unwrap();
    req.into_parts().0
}

/// A leader that cannot be reached is 502, one that does not answer
/// in time 504, and its answer otherwise passes through.
#[tokio::test]
async fn a_forwarded_request_fails_as_the_leader_does() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap().to_string();
    drop(closed);
    let r = relay(
        &addr,
        parts(),
        axum::body::Bytes::new(),
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);

    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = silent.local_addr().unwrap().to_string();
    let held = tokio::spawn(async move {
        let (tcp, _) = silent.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(tcp);
    });
    let r = relay(
        &addr,
        parts(),
        axum::body::Bytes::new(),
        Duration::from_millis(200),
    )
    .await;
    assert_eq!(r.status(), StatusCode::GATEWAY_TIMEOUT);
    held.abort();

    let answering = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = answering.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (mut tcp, _) = answering.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let n = tcp.read(&mut buf).await.unwrap();
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(head.contains(&format!("{FORWARDED}: 1")), "{head}");
        tcp.write_all(b"HTTP/1.1 423 Locked\r\ncontent-length: 2\r\n\r\n{}")
            .await
            .unwrap();
    });
    let r = relay(
        &addr,
        parts(),
        axum::body::Bytes::from_static(b"{}"),
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(r.status(), StatusCode::LOCKED);
}

#[test]
fn every_layout_names_its_legs() {
    assert_eq!(symbols("mono", 1), ["M1"]);
    assert_eq!(symbols("stereo", 2), ["L", "R"]);
    assert_eq!(symbols("7.1", 8).len(), 8);
}
