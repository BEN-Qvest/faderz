use super::*;
use crate::shape::tests::small;

fn with(replica: Value) -> String {
    json!({ "replica": replica, "shape": small() }).to_string()
}

#[test]
fn the_replica_is_named_after_the_host_unless_configured() {
    let (_, _, name) = Config::parse(&with(json!({})), || "faderz-0".into()).unwrap();
    assert_eq!(name, "faderz-0");
    let (_, _, name) = Config::parse(&with(json!({ "name": "r1" })), || "x".into()).unwrap();
    assert_eq!(name, "r1");
}

fn set_of(replica: Value, host: fn() -> String) -> Result<Option<Set>, String> {
    Config::parse(&with(replica), host).map(|(c, _, _)| c.set)
}

fn three(extra: Value) -> Value {
    let mut v = json!({
        "replicas": 3,
        "peers": ["f-0.f:7443", "f-1.f:7443", "f-2.f:7443"],
        "peer": { "bind": "0.0.0.0", "port": 7443, "tls": {
            "mode": "files", "certFile": "/t/tls.crt", "keyFile": "/t/tls.key",
            "clientCaFile": "/t/ca.crt" } },
        "marker": "faderz-replica-cluster",
    });
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    v
}

/// A StatefulSet pod named `<set>-<ordinal>` is that ordinal, and
/// reaches the others at the peer addresses listed by ordinal.
#[test]
fn a_set_takes_its_ordinal_from_the_pod_name() {
    let set = set_of(three(json!({})), || "faderz-2".into())
        .unwrap()
        .unwrap();
    assert_eq!((set.ordinal, set.voters.replicas), (2, 3));
    assert_eq!(set.addr(), "f-2.f:7443");
    let set = set_of(three(json!({ "ordinal": 1 })), || "faderz-2".into())
        .unwrap()
        .unwrap();
    assert_eq!(set.ordinal, 1, "an explicit ordinal wins");
}

/// The `replicas` message names every replica as clients see it named:
/// its pod's name, the StatefulSet's and its ordinal; without a
/// StatefulSet, the first label of its peer host; this one by its own
/// name.
#[test]
fn every_replica_of_a_set_is_named_as_its_pod_is() {
    use crate::app::member_names;
    let set = set_of(three(json!({})), || "mix-1".into())
        .unwrap()
        .unwrap();
    assert_eq!(member_names(&set, "mix-1"), ["mix-0", "mix-1", "mix-2"]);
    let set = set_of(three(json!({ "name": "desk", "ordinal": 2 })), || {
        "x".into()
    })
    .unwrap()
    .unwrap();
    assert_eq!(set.statefulset, None);
    assert_eq!(
        member_names(&set, "desk"),
        ["f-0.f:7443", "f-1.f:7443", "desk"]
    );
}

/// Without a StatefulSet each other replica is its whole peer
/// address, entry n for `peers[n]`: IP peers and bracketed IPv6 ones
/// stay distinct and the same on every replica, and a configured name
/// that is not `<set>-<n>` takes only its own slot.
#[test]
fn replica_ids_are_unique_whatever_the_peer_addresses_are() {
    use crate::app::member_names;
    let mut cfg = three(json!({ "name": "desk", "ordinal": 1 }));
    for peers in [
        ["10.0.0.1:80", "10.0.0.2:80", "10.0.0.3:80"],
        ["[::1]:80", "[::2]:80", "[::3]:80"],
        ["a.x:80", "b.x:80", "a.y:80"],
        ["h:80", "h:81", "h:82"],
    ] {
        cfg["peers"] = json!(peers);
        let set = set_of(cfg.clone(), || "x".into()).unwrap().unwrap();
        let names = member_names(&set, "desk");
        assert_eq!(names.len(), 3);
        for (n, name) in names.iter().enumerate() {
            let expect = if n == 1 { "desk" } else { peers[n] };
            assert_eq!(name, expect, "ordinal {n} is peers[{n}]");
        }
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 3, "{names:?}");
    }
}

/// One replica is a set of one, as before; it takes none of a set's
/// keys, so a half-configured set does not start as one alone.
#[test]
fn one_replica_is_a_set_of_one_and_takes_no_set_keys() {
    assert!(set_of(json!({}), || "faderz-0".into()).unwrap().is_none());
    let e = set_of(json!({ "marker": "m" }), || "x".into()).unwrap_err();
    assert!(e.contains("replica.marker"), "{e}");
}

#[test]
fn a_set_that_cannot_work_is_refused() {
    let host = || "faderz-0".to_owned();
    let refused = |v: Value, key: &str| {
        let e = set_of(v, host).unwrap_err();
        assert!(e.contains(key), "{key}: {e}");
    };
    let mut even = three(json!({ "replicas": 2 }));
    even["peers"] = json!(["a:1", "b:1"]);
    refused(even, "replica.replicas");
    refused(three(json!({ "peers": ["a:1", "b:1"] })), "replica.peers");
    refused(three(json!({ "ordinal": 3 })), "replica.ordinal");
    let e = set_of(three(json!({})), || "faderz".into()).unwrap_err();
    assert!(e.contains("replica.ordinal"), "{e}");
    // A name every pod would read alike: the ordinal is not taken
    // from it.
    refused(three(json!({ "name": "faderz-1" })), "replica.ordinal");
    let named = set_of(three(json!({ "name": "faderz-1", "ordinal": 1 })), host);
    assert_eq!(named.unwrap().unwrap().ordinal, 1);
    for (i, bad) in [
        (1, "f-1.f"),
        (1, ":7443"),
        (1, "f-1.f:0"),
        (2, "f-0.f:7443"),
    ] {
        let mut peers = three(json!({}));
        peers["peers"][i] = json!(bad);
        refused(peers, &format!("replica.peers[{i}]"));
    }
    let mut port0 = three(json!({}));
    port0["peer"]["port"] = json!(0);
    refused(port0, "replica.peer.port");
    let mut plain = three(json!({}));
    plain["peer"]["tls"] = json!({ "mode": "none" });
    refused(plain, "replica.peer.tls");
    let mut no_ca = three(json!({}));
    no_ca["peer"]["tls"]["clientCaFile"] = Value::Null;
    refused(no_ca, "replica.peer.tls");
    let mut no_marker = three(json!({}));
    no_marker.as_object_mut().unwrap().remove("marker");
    refused(no_marker, "replica.marker");
    let mut no_peer = three(json!({}));
    no_peer.as_object_mut().unwrap().remove("peer");
    refused(no_peer, "replica.peer");
}

/// A set's configuration with an `mxl` section whose output names
/// `output`.
fn set_with_mxl(output: Value) -> String {
    let mut v: Value = serde_json::from_str(&with(three(json!({})))).unwrap();
    v["mxl"] = json!({
        "domains": [{ "name": "d", "path": "/dev/shm/d" }],
        "inputs": { "ch1": { "flow": id(1), "altFlow": id(5) }, "ch2": { "flow": id(2) },
                    "st1": { "flow": id(3) } },
        "outputs": { "out-pgm": output },
    });
    v.to_string()
}

fn id(n: u8) -> String {
    format!("00000000-0000-4000-8000-00000000000{n}")
}

/// Each replica of a set runs an engine writing the member flow its
/// ordinal names, under the cap its configuration sets, and knows
/// its StatefulSet from its pod name and its revision from the
/// environment.
#[test]
fn each_replica_of_a_set_writes_its_own_member_flow() {
    let text = set_with_mxl(json!({ "flows": [id(4), id(6), id(7)] }));
    let env = |k: &str| (k == REVISION_ENV).then(|| "faderz-5d8f7c".to_owned());
    let hosts: [(fn() -> String, String); 3] = [
        (|| "faderz-0".into(), id(4)),
        (|| "faderz-1".into(), id(6)),
        (|| "faderz-2".into(), id(7)),
    ];
    for (n, (host, flow)) in hosts.into_iter().enumerate() {
        let (config, _, _) = Config::parse_with(&text, &[], env, host).unwrap();
        let engine = config.audio.as_ref().expect("an engine");
        assert_eq!(engine.outputs[0].flow_id, flow);
        // The member flow says whose it is.
        assert_eq!(engine.outputs[0].label, format!("PGM out (replica {n})"));
        let set = config.set.unwrap();
        assert_eq!(set.statefulset.as_deref(), Some("faderz"));
        assert_eq!(set.revision.as_deref(), Some("faderz-5d8f7c"));
        assert_eq!(set.max_apply_lead, std::time::Duration::from_millis(50));
    }
    let named = |name: &str, ordinal: u16| {
        let mut v: Value = serde_json::from_str(&text).unwrap();
        v["replica"]["name"] = json!(name);
        v["replica"]["ordinal"] = json!(ordinal);
        let (config, _, _) = Config::parse(&v.to_string(), || unreachable!()).unwrap();
        config.set.unwrap().statefulset
    };
    assert_eq!(named("mixer-a-2", 2).as_deref(), Some("mixer-a"));
    assert_eq!(named("mixer-2", 1), None, "a name of another ordinal");
    assert_eq!(named("mixer", 1), None);
}

/// Every replica of a set has the same compatibility key, though
/// each writes its own member flows; an engine setting or a flow
/// channel selection changes it.
#[test]
fn replicas_of_a_set_share_a_compatibility_key() {
    let text = set_with_mxl(json!({ "flows": [id(4), id(6), id(7)] }));
    let key = |text: &str, host: fn() -> String| {
        let (config, _, _) = Config::parse(text, host).unwrap();
        config.compatibility(7)
    };
    let zero = key(&text, || "faderz-0".into());
    assert_eq!(zero, key(&text, || "faderz-2".into()));
    let (config, _, _) = Config::parse(&text, || "faderz-0".into()).unwrap();
    assert_ne!(
        zero,
        config.compatibility(8),
        "another product configuration"
    );
    let mut v: Value = serde_json::from_str(&text).unwrap();
    v["engine"] = json!({ "rampSamples": 960 });
    assert_ne!(zero, key(&v.to_string(), || "faderz-0".into()));
    let mut v: Value = serde_json::from_str(&text).unwrap();
    v["mxl"]["inputs"]["st1"]["flow"] = json!(id(8));
    v["mxl"]["inputs"]["st1"]["channels"] = json!([2, 1]);
    assert_ne!(zero, key(&v.to_string(), || "faderz-0".into()));
}

/// A set of one writes `flow`, a set of more one member flow per
/// replica in `flows`, no two outputs or replicas the same one and
/// none an input reads; samplez checks the flows of every ordinal,
/// not only this replica's.
#[test]
fn member_flows_are_checked() {
    let refused = |output: Value, key: &str| {
        let e = Config::parse(&set_with_mxl(output), || "faderz-0".into()).unwrap_err();
        assert!(e.starts_with(key), "{key}: {e}");
    };
    refused(json!({ "flow": id(4) }), "mxl.outputs.out-pgm.flow");
    refused(json!({}), "mxl.outputs.out-pgm.flows");
    refused(
        json!({ "flows": [id(4), id(6)] }),
        "mxl.outputs.out-pgm.flows",
    );
    refused(json!({ "flows": [id(4), id(6), id(4)] }), "mxl.outputs:");
    refused(json!({ "flows": [id(4), id(6), id(1)] }), "mxl.outputs:");
    refused(json!({ "flows": [id(4), id(6), id(5)] }), "mxl.outputs:");
    refused(
        json!({ "flows": [id(4), id(6), "x"] }),
        "engine of replica ordinal 2",
    );
    let mut one: Value = serde_json::from_str(&set_with_mxl(json!({}))).unwrap();
    one["replica"] = json!({ "name": "r1" });
    one["mxl"]["outputs"]["out-pgm"] = json!({ "flows": [id(4)] });
    let e = Config::parse(&one.to_string(), || unreachable!()).unwrap_err();
    assert!(e.starts_with("mxl.outputs.out-pgm.flows"), "{e}");
    one["mxl"]["outputs"]["out-pgm"] = json!({ "flow": id(4) });
    let (config, _, _) = Config::parse(&one.to_string(), || unreachable!()).unwrap();
    let audio = config.audio.unwrap();
    let output = &audio.outputs[0];
    assert_eq!(output.flow_id, id(4));
    assert_eq!(
        output.label, "PGM out",
        "a set of one writes the merged flow"
    );
}

/// The marker's name can come from the environment.
#[test]
fn the_marker_comes_from_the_environment() {
    let env = |k: &str| (k == "FADERZ_REPLICA_MARKER").then(|| "faderz-replica-cluster".to_owned());
    let mut v = three(json!({}));
    v.as_object_mut().unwrap().remove("marker");
    let text = with(v);
    let (config, _, _) = Config::parse_with(&text, &[], env, || "faderz-0".into()).unwrap();
    assert_eq!(config.set.unwrap().marker, "faderz-replica-cluster");
    // A set of one passes over it, and says so.
    let text = with(json!({}));
    let (config, _, _) = Config::parse_with(&text, &[], env, || "faderz-0".into()).unwrap();
    assert!(config.set.is_none());
    assert_eq!(
        config.notes,
        ["FADERZ_REPLICA_MARKER ignored: replica.replicas is 1"]
    );
}

/// Proxy mode serves the API to the proxy beside it alone.
#[test]
fn proxy_mode_binds_the_api_to_loopback_only() {
    let config = |bind: &str| {
        json!({
            "listeners": { "api": { "bind": bind, "port": 8080 } },
            "auth": { "mode": "proxy" },
            "replica": { "name": "r1" },
            "shape": small(),
        })
        .to_string()
    };
    let e = Config::parse(&config("0.0.0.0"), || unreachable!()).unwrap_err();
    assert!(e.contains("loopback"), "{e}");
    assert!(Config::parse(&config("10.0.0.5"), || unreachable!()).is_err());
    assert!(Config::parse(&config("127.0.0.1"), || unreachable!()).is_ok());
    assert!(Config::parse(&config("::1"), || unreachable!()).is_ok());
}

fn oidc_flags() -> Vec<(String, String)> {
    let args: Vec<String> = [
        "--config",
        "/etc/faderz/config.json",
        "--auth-mode",
        "oidc",
        "--oidc-issuer-url=https://idp.example/realms/media",
        "--oidc-client-id",
        "faderz",
        "--oidc-public-url",
        "https://faderz.example",
        "--oidc-scopes",
        "profile, groups",
    ]
    .map(String::from)
    .into();
    match command(&args).unwrap() {
        Command::Run { config, flags } => {
            assert_eq!(config, PathBuf::from("/etc/faderz/config.json"));
            flags
        }
        other => panic!("{other:?}"),
    }
}

/// Every OIDC setting the file takes, the command line and the
/// environment take as well; the flag wins over the variable, the
/// variable over the file.
#[test]
fn oidc_settings_come_from_file_flags_or_environment() {
    let file = json!({
        "replica": { "name": "r1" },
        "shape": small(),
        "auth": { "oidc": { "clientId": "from-file", "roleClaim": "roles" } },
    })
    .to_string();
    let env = |k: &str| match k {
        "FADERZ_OIDC_CLIENT_ID" => Some("from-env".to_owned()),
        "FADERZ_OIDC_VIEWER_ROLE" => Some("faderz-viewer".to_owned()),
        "FADERZ_OIDC_SESSION_KEY_FILE" => Some("/run/secrets/key".to_owned()),
        "FADERZ_OIDC_AUDIENCE" => Some(String::new()),
        _ => None,
    };
    let (config, _, _) = Config::parse_with(&file, &oidc_flags(), env, || unreachable!()).unwrap();
    assert_eq!(config.auth.mode, AuthMode::Oidc);
    let o = config.auth.oidc.unwrap();
    assert_eq!(o.client_id, "faderz", "the flag over the variable");
    assert_eq!(o.issuer_url, "https://idp.example/realms/media");
    assert_eq!(o.public_url, "https://faderz.example");
    assert_eq!(o.scopes, ["profile", "groups"]);
    assert_eq!(o.role_claim, "roles", "the file where nothing else sets it");
    assert_eq!(o.viewer_role.as_deref(), Some("faderz-viewer"));
    assert_eq!(o.audience, None, "an empty variable is unset");
    assert_eq!(
        o.session_key.unwrap().file,
        PathBuf::from("/run/secrets/key")
    );
    assert_eq!(o.operator_role, DEFAULT_OPERATOR_ROLE);

    let flags = [
        ("--auth-mode", "oidc"),
        ("--oidc-client-secret-file", "/run/s"),
        ("--oidc-issuer-url", "https://idp.example"),
        ("--oidc-public-url", "https://faderz.example"),
    ]
    .map(|(f, v)| (f.to_owned(), v.to_owned()));
    let (config, _, _) = Config::parse_with(&file, &flags, |_| None, || unreachable!()).unwrap();
    let o = config.auth.oidc.unwrap();
    assert_eq!(o.client_id, "from-file");
    assert_eq!(o.client_secret.unwrap().file, PathBuf::from("/run/s"));

    // Every setting maps onto a key the file takes.
    let all: Vec<(String, String)> = SETTINGS
        .iter()
        .map(|s| (s.flag.to_owned(), "x".to_owned()))
        .collect();
    let mut v = json!({});
    apply(&mut v, &all, |_| None).unwrap();
    v["auth"]["mode"] = json!("oidc");
    serde_json::from_value::<AuthConfig>(v["auth"].clone()).unwrap();
}

/// The operator role is defaulted for oidc mode only: a stray
/// `auth.oidc` section in another mode is not made valid by it.
#[test]
fn the_operator_role_defaults_in_oidc_mode_only() {
    let config = |mode: &str| {
        json!({
            "replica": { "name": "r1" },
            "shape": small(),
            "auth": { "mode": mode, "token": { "file": "/run/t" }, "oidc": {
                "issuerUrl": "https://idp.example", "clientId": "c",
                "publicUrl": "https://faderz.example",
            } },
        })
        .to_string()
    };
    let (c, _, _) = Config::parse(&config("oidc"), || unreachable!()).unwrap();
    assert_eq!(c.auth.oidc.unwrap().operator_role, DEFAULT_OPERATOR_ROLE);
    let e = Config::parse(&config("token"), || unreachable!()).unwrap_err();
    assert!(e.contains("operatorRole"), "{e}");
    let flags = [("--auth-mode".to_owned(), "oidc".to_owned())];
    let parsed = Config::parse_with(&config("token"), &flags, |_| None, || unreachable!());
    assert!(parsed.is_ok(), "the mode a flag sets counts");
}

#[test]
fn the_auth_mode_names_where_it_came_from() {
    let flag = [("--auth-mode".to_owned(), "none".to_owned())];
    let env = |k: &str| (k == "FADERZ_AUTH_MODE").then(|| "token".to_owned());
    assert_eq!(auth_mode_source(&flag, env), "--auth-mode");
    assert_eq!(auth_mode_source(&[], env), "FADERZ_AUTH_MODE");
    assert_eq!(auth_mode_source(&[], |_| None), "the configuration file");
    let empty = |_: &str| Some(String::new());
    assert_eq!(auth_mode_source(&[], empty), "the configuration file");
}

#[test]
fn the_command_line_is_checked() {
    let args = |a: &[&str]| command(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(args(&["--version"]), Ok(Command::Version));
    assert_eq!(args(&["--help"]), Ok(Command::Help));
    assert!(args(&[]).is_err());
    assert!(
        args(&["--oidc-client-id", "x"])
            .unwrap_err()
            .contains("--config")
    );
    assert!(args(&["--config"]).is_err());
    assert!(
        args(&["--config", "c", "--oidc-nope", "x"])
            .unwrap_err()
            .contains("unknown")
    );
    assert!(args(&["--config", "c", "--oidc-client-id"]).is_err());
    assert!(args(&["--config", "c", "--version"]).is_err());
    let help = usage();
    for s in SETTINGS {
        assert!(help.contains(s.flag) && help.contains(s.env), "{}", s.flag);
    }
}

/// A name clients could not be told (`ReplicaRef.id`) refuses the
/// start rather than every message later.
#[test]
fn a_name_that_is_not_an_id_is_refused() {
    let e = Config::parse(&with(json!({})), || "Host.example".into()).unwrap_err();
    assert!(e.contains("replica.name"), "{e}");
}

/// A channel's input configured `tone` is a samplez tone generator:
/// no flow, no domain, even beside two domains, and wired to no flow;
/// the shape carries the frequency and level it starts at, samplez's
/// 1 kHz at -18 dBFS unless given, and the initial state its tone. A
/// flow, input b, a domain or a flow channel selection beside it is
/// refused, as are start values outside samplez's ranges, a channel
/// with altInput, a channel with neither flow nor tone, and a shape
/// that sets `tone` itself.
#[test]
fn a_tone_input_reads_no_flow() {
    let id = |n: u8| format!("00000000-0000-4000-8000-00000000000{n}");
    let config = |st1: Value, domains: Value| {
        json!({
            "replica": { "name": "r1" },
            "shape": small(),
            "mxl": {
                "domains": domains,
                "inputs": {
                    "ch1": { "flow": id(1), "altFlow": id(5), "domain": "d" },
                    "ch2": { "flow": id(2), "domain": "d" },
                    "st1": st1,
                },
                "outputs": { "out-pgm": { "flow": id(4), "domain": "d" } },
            },
        })
        .to_string()
    };
    let two = json!([
        { "name": "d", "path": "/dev/shm/d" },
        { "name": "e", "path": "/dev/shm/e" },
    ]);
    let parse = |st1: Value| Config::parse(&config(st1, two.clone()), || unreachable!());

    let (c, shape, _) = parse(json!({ "tone": {} })).unwrap();
    let engine = c.audio.as_ref().unwrap();
    assert!(engine.inputs[2].tone.is_some());
    assert_eq!(
        (&engine.inputs[2].flow_id, &engine.inputs[2].domain),
        (&None, &None)
    );
    assert!(engine.inputs[..2].iter().all(|i| i.tone.is_none()));
    assert_eq!(
        c.wiring[2],
        Wiring {
            a: String::new(),
            b: None,
            domain: String::new(),
        }
    );
    let start = samplez_dsp::ToneParams::DEFAULT;
    assert_eq!(
        shape.channels[2].tone,
        Some(ChannelTone {
            frequency: start.frequency_hz,
            level: f64::from(start.level_db),
        })
    );
    assert_eq!(shape.channels[0].tone, None);
    let state = shape.initial_state();
    assert_eq!(
        state["channels"]["st1"]["tone"],
        json!({ "frequency": 1000, "level": -18.0, "ident": false })
    );
    assert!(state["channels"]["ch1"].get("tone").is_none());

    let (_, shape, _) = parse(json!({ "tone": { "frequency": 440, "level": -20.5 } })).unwrap();
    assert_eq!(
        shape.channels[2].tone,
        Some(ChannelTone {
            frequency: 440,
            level: -20.5,
        })
    );
    // The ends of samplez's ranges are taken.
    for (frequency, level) in [(20, -60.0), (20_000, 0.0)] {
        let tone = json!({ "tone": { "frequency": frequency, "level": level } });
        parse(tone).unwrap();
    }

    for key in ["flow", "altFlow", "domain", "channels"] {
        let value = if key == "channels" {
            json!([1, 2])
        } else if key == "domain" {
            json!("d")
        } else {
            json!(id(6))
        };
        let mut st1 = json!({ "tone": {} });
        st1[key] = value;
        let e = parse(st1).unwrap_err();
        assert_eq!(
            e,
            format!("mxl.inputs.st1.{key}: a tone input reads no flow")
        );
    }
    for (tone, refused) in [
        (json!({ "frequency": 19 }), "frequency 19"),
        (json!({ "frequency": 20_001 }), "frequency 20001"),
        (json!({ "level": 0.001 }), "level 0.001"),
        (json!({ "level": -60.001 }), "level -60.001"),
    ] {
        let e = parse(json!({ "tone": tone })).unwrap_err();
        assert!(
            e.starts_with(&format!("mxl.inputs.st1.tone.{refused}: not in")),
            "{e}"
        );
    }
    assert!(parse(json!({ "tone": { "ident": true } })).is_err());
    let e = parse(json!({})).unwrap_err();
    assert!(
        e.contains("mxl.inputs.st1.flow: required without tone"),
        "{e}"
    );

    let mut alt = json!({
        "replica": { "name": "r1" },
        "shape": small(),
        "mxl": {
            "domains": [{ "name": "d", "path": "/dev/shm/d" }],
            "inputs": {
                "ch1": { "tone": {} },
                "ch2": { "flow": id(2) },
                "st1": { "flow": id(3) },
            },
            "outputs": { "out-pgm": { "flow": id(4) } },
        },
    });
    let e = Config::parse(&alt.to_string(), || unreachable!()).unwrap_err();
    assert!(
        e.contains("mxl.inputs.ch1.tone: the channel has altInput"),
        "{e}"
    );
    alt["shape"]["channels"][0]["altInput"] = json!(false);
    Config::parse(&alt.to_string(), || unreachable!()).unwrap();
    alt["shape"]["channels"][1]["tone"] = json!({ "frequency": 1000, "level": -18 });
    let e = Config::parse(&alt.to_string(), || unreachable!()).unwrap_err();
    assert!(
        e.contains("channel ch2 tone: set by mxl.inputs.ch2.tone"),
        "{e}"
    );
}

/// Every channel the state lets select input b has a flow for it.
#[test]
fn an_alt_input_needs_an_alt_flow() {
    let id = |n: u8| format!("00000000-0000-4000-8000-00000000000{n}");
    let config = |ch1: Value| {
        json!({
            "replica": { "name": "r1" },
            "shape": small(),
            "mxl": {
                "domains": [{ "name": "d", "path": "/dev/shm/d" }],
                "inputs": { "ch1": ch1, "ch2": { "flow": id(2) }, "st1": { "flow": id(3) } },
                "outputs": { "out-pgm": { "flow": id(4) } },
            },
        })
        .to_string()
    };
    let e = Config::parse(&config(json!({ "flow": id(1) })), || unreachable!()).unwrap_err();
    assert!(e.contains("mxl.inputs.ch1.altFlow: required"), "{e}");
    let ok = config(json!({ "flow": id(1), "altFlow": id(5) }));
    Config::parse(&ok, || unreachable!()).unwrap();

    // Each bus, aux and mix-minus output no full-width output carries,
    // and each monitor output, is an engine meter on matrix columns
    // after the outputs', and each leg is weighted for loudness by its
    // layout.
    let (c, ..) = Config::parse(&ok, || unreachable!()).unwrap();
    let e = c.audio.unwrap();
    let meters: Vec<_> = e
        .meters
        .iter()
        .map(|m| (m.name.as_str(), m.channels))
        .collect();
    assert_eq!(meters, [("cf", 2), ("aux1", 1), ("mm1", 2), ("mon", 2)]);
    assert_eq!((e.columns(), e.matrix[0].len()), (9, 9));
    // The shape has a duck group: every column but a direct out's
    // may be ducked.
    assert_eq!(e.duck_columns, (0..9).collect::<Vec<_>>());
    let stereo = Some(vec!["L".to_owned(), "R".to_owned()]);
    assert_eq!(e.inputs[2].loudness_channels, stereo);
    assert_eq!(e.meters[0].loudness_channels, stereo);
    assert_eq!(e.reader_margin_samples, READER_MARGIN);

    let mut set: Value = serde_json::from_str(&ok).unwrap();
    set["engine"] = json!({ "readerMarginSamples": 960 });
    let (c, ..) = Config::parse(&set.to_string(), || unreachable!()).unwrap();
    assert_eq!(c.audio.unwrap().reader_margin_samples, 960);

    // The engine and the replica's peer connections take one delay
    // budget: the configured one, else DELAY_BUDGET.
    let (c, ..) = Config::parse(&ok, || unreachable!()).unwrap();
    assert_eq!(c.delay_budget(), DELAY_BUDGET);
    assert_eq!(c.audio.unwrap().delay_budget_samples, DELAY_BUDGET);
    set["engine"] = json!({ "delayBudgetSamples": 480_000 });
    let (c, ..) = Config::parse(&set.to_string(), || unreachable!()).unwrap();
    assert_eq!(c.delay_budget(), 480_000);
    assert_eq!(c.audio.unwrap().delay_budget_samples, 480_000);
}

/// A channel's delay line is opt-in, given in ms and taken in samples
/// at the shape's rate, by the shape (`maxDelay`) and the engine
/// (`maxDelaySamples`) alike. Lines that together hold more than the
/// budget refuse the start, naming each channel's share; one past
/// what the engine takes, or one written into the shape, too.
#[test]
fn delay_lines_are_opt_in_and_within_the_budget() {
    let id = |n: u8| format!("00000000-0000-4000-8000-00000000000{n}");
    let config = |ch1: Value, st1: Value, engine: Value| {
        json!({
            "replica": { "name": "r1" },
            "shape": small(),
            "engine": engine,
            "mxl": {
                "domains": [{ "name": "d", "path": "/dev/shm/d" }],
                "inputs": {
                    "ch1": { "flow": id(1), "altFlow": id(5), "maxDelayMs": ch1 },
                    "ch2": { "flow": id(2) },
                    "st1": { "flow": id(3), "maxDelayMs": st1 },
                },
                "outputs": { "out-pgm": { "flow": id(4) } },
            },
        })
        .to_string()
    };
    let (c, shape, _) = Config::parse(
        &config(json!(20), json!(5000), json!({})),
        || unreachable!(),
    )
    .unwrap();
    let lines: Vec<Option<u32>> = shape.channels.iter().map(|c| c.max_delay).collect();
    assert_eq!(lines, [Some(960), None, Some(240_000)]);
    let engine: Vec<u32> = (c.audio.unwrap().inputs.iter())
        .map(|i| i.max_delay_samples)
        .collect();
    // The line-up tone's mono and stereo inputs last, which have no
    // line.
    assert_eq!(engine, [960, 0, 240_000, 0, 0]);
    // 0 is no line.
    let (_, shape, _) =
        Config::parse(&config(json!(0), json!(0), json!({})), || unreachable!()).unwrap();
    assert!(shape.channels.iter().all(|c| c.max_delay.is_none()));

    // The stereo line counts twice.
    let held = 960 + 2 * 240_000;
    let fits = json!({ "delayBudgetSamples": held });
    Config::parse(&config(json!(20), json!(5000), fits), || unreachable!()).unwrap();
    let short = json!({ "delayBudgetSamples": held - 1 });
    let e = Config::parse(&config(json!(20), json!(5000), short), || unreachable!()).unwrap_err();
    assert!(e.contains("ch1 (960), st1 (480000)"), "{e}");
    assert!(
        e.contains(&format!("delayBudgetSamples {}", held - 1)),
        "{e}"
    );

    let e = Config::parse(
        &config(json!(20), json!(5000.1), json!({})),
        || unreachable!(),
    )
    .unwrap_err();
    assert!(e.contains("mxl.inputs.st1.maxDelayMs 5000.1"), "{e}");
    let e = Config::parse(&config(json!(-1), json!(0), json!({})), || unreachable!()).unwrap_err();
    assert!(e.contains("mxl.inputs.ch1.maxDelayMs -1"), "{e}");
    // A line too short for one sample is refused, not taken as none.
    let e =
        Config::parse(&config(json!(0.01), json!(0), json!({})), || unreachable!()).unwrap_err();
    assert!(
        e.contains("mxl.inputs.ch1.maxDelayMs 0.01: under one sample"),
        "{e}"
    );

    let mut set: Value = serde_json::from_str(&config(json!(0), json!(0), json!({}))).unwrap();
    set["shape"]["channels"][0]["maxDelay"] = json!(960);
    let e = Config::parse(&set.to_string(), || unreachable!()).unwrap_err();
    assert!(e.contains("set by mxl.inputs.ch1.maxDelayMs"), "{e}");
}

/// Declared consoles take defaults; a console the contract refuses or
/// an id used twice refuses the configuration.
#[test]
fn consoles_are_checked_and_take_defaults() {
    let config = |consoles: Value| {
        json!({ "replica": { "name": "r1" }, "shape": small(), "consoles": consoles }).to_string()
    };
    let (c, ..) = Config::parse(
        &config(json!([
            { "id": "xt-1", "name": "X-Touch" },
            { "id": "osc-1", "name": "Tablet", "protocol": "osc", "transport": "osc-udp", "strips": 16 },
        ])),
        || unreachable!(),
    )
    .unwrap();
    assert_eq!(
        (
            c.consoles[0].protocol.as_str(),
            c.consoles[0].transport.as_str(),
            c.consoles[0].strips
        ),
        ("mcu", "bridge", 8)
    );
    assert_eq!(c.consoles[1].strips, 16);
    let (c, ..) = Config::parse(&with(json!({ "name": "r1" })), || unreachable!()).unwrap();
    assert!(c.consoles.is_empty());
    let refused =
        |consoles: Value| Config::parse(&config(consoles), || unreachable!()).unwrap_err();
    let e = refused(json!([{ "id": "a", "name": "A" }, { "id": "a", "name": "B" }]));
    assert_eq!(e, "consoles[1].id \"a\": used twice");
    let e = refused(json!([{ "id": "Bad", "name": "A" }]));
    assert!(e.contains("consoles[0]"), "{e}");
    let e = refused(json!([{ "id": "a", "name": "" }]));
    assert!(e.contains("consoles[0]"), "{e}");
    let e = refused(json!([{ "id": "a", "name": "A", "strips": 0 }]));
    assert!(e.contains("consoles[0]"), "{e}");
    let e = refused(json!([{ "id": "a", "name": "A", "protocol": "midi" }]));
    assert!(e.contains("consoles[0]"), "{e}");
    let e = refused(json!([{ "id": "a", "name": "A", "bankOffset": 0 }]));
    assert!(e.contains("bankOffset"), "{e}");
}

/// A channel's 1-based flow channels reach the engine 0-based, one
/// per leg, and label the channel's source; a selection of the wrong
/// length or out of range is refused.
#[test]
fn a_channel_selects_its_legs_from_the_flow() {
    let id = |n: u8| format!("00000000-0000-4000-8000-00000000000{n}");
    let config = |st1: Value, source: Option<&str>| {
        let mut shape = small();
        if let Some(s) = source {
            shape["channels"][2]["source"] = json!(s);
        }
        json!({
            "replica": { "name": "r1" },
            "shape": shape,
            "mxl": {
                "domains": [{ "name": "d", "path": "/dev/shm/d" }],
                "inputs": {
                    "ch1": { "flow": id(1), "altFlow": id(5), "channels": [16] },
                    "ch2": { "flow": id(2) },
                    "st1": st1,
                },
                "outputs": { "out-pgm": { "flow": id(4) } },
            },
        })
        .to_string()
    };
    assert_eq!(MAX_FLOW_CHANNEL, samplez_mxl::input::MAX_FLOW_CHANNELS);
    let parse = |st1: Value, source| Config::parse(&config(st1, source), || unreachable!());
    let (c, shape, _) = parse(json!({ "flow": id(3), "channels": [7, 8] }), None).unwrap();
    let e = c.audio.unwrap();
    assert_eq!(e.inputs[0].flow_channels, Some(vec![15]));
    assert_eq!(e.inputs[1].flow_channels, None);
    assert_eq!(e.inputs[2].flow_channels, Some(vec![6, 7]));
    assert_eq!(e.inputs[2].channels, 2);
    let sources: Vec<_> = shape.channels.iter().map(|c| c.source.as_deref()).collect();
    assert_eq!(sources, [Some("ch 16"), None, Some("ch 7-8")]);
    check(&contract().shape, &serde_json::to_value(&shape).unwrap()).unwrap();

    let (_, shape, _) = parse(
        json!({ "flow": id(3), "channels": [2, 1] }),
        Some("Stagebox"),
    )
    .unwrap();
    assert_eq!(shape.channels[2].source.as_deref(), Some("Stagebox ch 2,1"));
    let (_, shape, _) = parse(json!({ "flow": id(3), "channels": [3, 3] }), None).unwrap();
    assert_eq!(shape.channels[2].source.as_deref(), Some("ch 3,3"));

    for (bad, why) in [
        (json!([7]), "names 1 flow channels, the channel is 2 wide"),
        (json!([7, 8, 9]), "names 3 flow channels"),
        (json!([0, 1]), "flow channel 0 is not in 1..=64"),
        (json!([1, 65]), "flow channel 65 is not in 1..=64"),
    ] {
        let e = parse(json!({ "flow": id(3), "channels": bad }), None).unwrap_err();
        assert!(
            e.contains("mxl.inputs.st1.channels: ") && e.contains(why),
            "{e}"
        );
    }
    let long = "x".repeat(MAX_SOURCE - 6);
    assert!(parse(json!({ "flow": id(3), "channels": [7, 8] }), Some(&long)).is_err());
    assert!(
        parse(
            json!({ "flow": id(3), "channels": [7, 8] }),
            Some(&long[1..])
        )
        .is_ok()
    );
}

/// More buses, auxes, mix-minus and monitor outputs without an output
/// than the engine has meters for refuse the configuration, naming
/// the limit; as many as it has are taken.
#[test]
fn more_uncarried_elements_than_engine_meters_are_refused() {
    let id = |n: u32| format!("00000000-0000-4000-8000-{n:012}");
    let config = |auxes: usize| {
        let mut shape = small();
        // cf, mm1 and mon have no full-width output: aux1 and the added auxes
        // make up the rest.
        let aux = |i: usize| json!({ "id": format!("aux{i}"), "name": "a", "width": 1, "layout": "mono" });
        shape["auxes"] = (1..=auxes).map(aux).collect();
        json!({
            "replica": { "name": "r1" },
            "shape": shape,
            "mxl": {
                "domains": [{ "name": "d", "path": "/dev/shm/d" }],
                "inputs": {
                    "ch1": { "flow": id(1), "altFlow": id(5) },
                    "ch2": { "flow": id(2) },
                    "st1": { "flow": id(3) },
                },
                "outputs": { "out-pgm": { "flow": id(4) } },
            },
        })
        .to_string()
    };
    let max = samplez::config::MAX_METERS;
    Config::parse(&config(max - 3), || unreachable!()).unwrap();
    let e = Config::parse(&config(max - 2), || unreachable!()).unwrap_err();
    assert!(e.contains(&format!("at most {max}")), "{e}");
}

/// A monitor output takes an engine meter also while a full-width
/// output carries it, so it counts against the engine's meters: a
/// shape whose meters exceed them is refused at its parse, naming
/// their count and the limit.
#[test]
fn a_carried_monitor_output_counts_against_the_engine_meters() {
    let id = |n: u32| format!("00000000-0000-4000-8000-{n:012}");
    let config = |auxes: usize| {
        let mut shape = small();
        shape["outputs"].as_array_mut().unwrap().push(json!(
            { "id": "out-mon", "name": "Mon", "source": "mon", "width": 2, "layout": "stereo" }
        ));
        // cf, mm1, the added auxes and mon are engine meters.
        let aux = |i: usize| json!({ "id": format!("aux{i}"), "name": "a", "width": 1, "layout": "mono" });
        shape["auxes"] = (1..=auxes).map(aux).collect();
        json!({
            "replica": { "name": "r1" },
            "shape": shape,
            "mxl": {
                "domains": [{ "name": "d", "path": "/dev/shm/d" }],
                "inputs": {
                    "ch1": { "flow": id(1), "altFlow": id(5) },
                    "ch2": { "flow": id(2) },
                    "st1": { "flow": id(3) },
                },
                "outputs": {
                    "out-pgm": { "flow": id(4) },
                    "out-mon": { "flow": id(6) },
                },
            },
        })
        .to_string()
    };
    let max = samplez::config::MAX_METERS;
    let (c, ..) = Config::parse(&config(max - 3), || unreachable!()).unwrap();
    let engine = c.audio.unwrap();
    assert_eq!(engine.meters.len(), max);
    assert_eq!(engine.meters[max - 1].name, "mon");
    let e = Config::parse(&config(max - 2), || unreachable!()).unwrap_err();
    assert!(
        e.starts_with(&format!("shape: {} engine meters", max + 1)),
        "{e}"
    );
    assert!(e.contains(&format!("at most {max}")), "{e}");
}

/// With duck groups, the monitor's meter columns count against
/// samplez's coefficient budget as ducked columns. A shape whose
/// matrix only they take over it refuses the configuration naming
/// them, the block size and the block size that fits, while the same
/// shape without the monitor output parses; a shape that does not fit
/// without them either keeps the engine's own refusal.
#[test]
fn monitor_meter_columns_over_the_coefficient_budget_are_named() {
    let id = |n: u32| format!("00000000-0000-4000-8000-{n:012}");
    // The small shape with `extra` mono channels more, a duck group and,
    // with `monitor`, its monitor output carried, at blockSize 16.
    let config = |extra: u32, monitor: bool| {
        let mut shape = small();
        shape["ducks"] = json!([{ "id": "d1", "name": "Duck" }]);
        let mut inputs = json!({
            "ch1": { "flow": id(1), "altFlow": id(5) },
            "ch2": { "flow": id(2) },
            "st1": { "flow": id(3) },
        });
        for n in 0..extra {
            let c = format!("x{n}");
            shape["channels"].as_array_mut().unwrap().push(
                json!({ "id": c, "name": c, "width": 1, "layout": "mono", "altInput": false }),
            );
            inputs[&c] = json!({ "flow": id(100 + n) });
        }
        let mut outputs = json!({ "out-pgm": { "flow": id(4) } });
        if monitor {
            shape["outputs"].as_array_mut().unwrap().push(json!(
                { "id": "out-mon", "name": "Mon", "source": "mon", "width": 2, "layout": "stereo" }
            ));
            outputs["out-mon"] = json!({ "flow": id(6) });
        } else {
            shape["monitorOutputs"] = json!([]);
        }
        json!({
            "replica": { "name": "r1" },
            "shape": shape,
            "engine": { "blockSize": 16 },
            "mxl": {
                "domains": [{ "name": "d", "path": "/dev/shm/d" }],
                "inputs": inputs,
                "outputs": outputs,
            },
        })
        .to_string()
    };
    let parse = |extra, monitor| Config::parse(&config(extra, monitor), || unreachable!());
    let named = "shape: the monitor meter columns";
    let extra = (0..60)
        .find(|&n| parse(n, true).is_err_and(|e| e.starts_with(named)))
        .expect("a channel count at which only the monitor meter columns do not fit");
    let e = parse(extra, true).unwrap_err();
    assert!(e.contains("at blockSize 16"), "{e}");
    assert!(e.contains("raise engine.blockSize to at least"), "{e}");
    parse(extra, false)
        .unwrap_or_else(|e| panic!("{extra} channels more without the monitor: {e}"));
    // Fewer channels fit with the monitor; more do not fit without it,
    // and the engine refuses them in its own words.
    parse(extra - 1, true).unwrap_or_else(|e| panic!("{} channels more: {e}", extra - 1));
    let over = (extra..120)
        .find(|&n| parse(n, false).is_err())
        .expect("a channel count over the budget without the monitor");
    let e = parse(over, true).unwrap_err();
    assert!(!e.starts_with(named), "{over} channels more: {e}");
}

/// NMOS settings that cannot work refuse the configuration: NMOS
/// without an engine, an advertised address, plain HTTP or a merged
/// flow where a set of more needs one.
#[test]
fn nmos_settings_that_cannot_work_are_refused() {
    let id = |n: u32| format!("00000000-0000-4000-8000-{n:012}");
    let config = |nmos: Value, out: Value, replica: Value| {
        json!({
            "replica": replica,
            "shape": small(),
            "listeners": { "nmos": { "bind": "0.0.0.0", "port": 8081 } },
            "mxl": {
                "domains": [{ "name": "d", "path": "/dev/shm/d", "id": id(9) }],
                "inputs": {
                    "ch1": { "flow": id(1), "altFlow": id(5) },
                    "ch2": { "flow": id(2) },
                    "st1": { "flow": id(3) },
                },
                "outputs": { "out-pgm": out },
            },
            "nmos": nmos,
        })
        .to_string()
    };
    let one = || json!({ "name": "r1" });
    let flow = || json!({ "flow": id(4) });
    let on = |extra: Value| {
        let mut v = json!({ "enabled": true, "advertiseHost": "10.0.0.1" });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    };
    let host = || "faderz-0".to_owned();
    let (c, ..) = Config::parse(&config(on(json!({})), flow(), one()), host).unwrap();
    assert_eq!(
        (c.nmos.heartbeat_s, c.nmos.asset.manufacturer.as_str()),
        (5, "danielclasen")
    );
    let refused = |text: String, key: &str| {
        let e = Config::parse(&text, host).unwrap_err();
        assert!(e.contains(key), "{key}: {e}");
    };
    let no_mxl = json!({ "replica": one(), "shape": small(), "nmos": on(json!({})) });
    refused(no_mxl.to_string(), "nmos.enabled");
    refused(
        config(json!({ "enabled": true }), flow(), one()),
        "nmos.advertiseHost",
    );
    let https = on(json!({ "registry": { "url": "https://registry:443" } }));
    refused(config(https, flow(), one()), "nmos.registry.url");
    refused(
        config(on(json!({ "heartbeatS": 11 })), flow(), one()),
        "nmos.heartbeatS",
    );
    let merged = json!({ "flow": id(4), "mergedFlow": id(6) });
    refused(config(json!({}), merged, one()), "mergedFlow");
    let set = || {
        json!({
            "replicas": 3,
            "peers": ["f-0.f:7443", "f-1.f:7443", "f-2.f:7443"],
            "peer": { "bind": "0.0.0.0", "port": 7443, "tls": {
                "mode": "files", "certFile": "/t/tls.crt", "keyFile": "/t/tls.key",
                "clientCaFile": "/t/ca.crt" } },
            "marker": "m",
        })
    };
    let members = || json!({ "flows": [id(4), id(7), id(8)] });
    refused(config(on(json!({})), members(), set()), "mergedFlow");
    let mut merged = members();
    merged["mergedFlow"] = json!(id(6));
    Config::parse(&config(on(json!({})), merged, set()), host).unwrap();
    // Without NMOS a set needs none.
    Config::parse(&config(json!({}), members(), set()), host).unwrap();
    // A merged flow no input reads: a receiver would feed it back.
    let mut fed = members();
    fed["mergedFlow"] = json!(id(1));
    refused(config(on(json!({})), fed, set()), "mxl.outputs");
    // The NMOS addresses of the others, one per replica.
    let mut merged = members();
    merged["mergedFlow"] = json!(id(6));
    let two = on(json!({ "peers": ["a:1", "b:1"] }));
    refused(config(two, merged.clone(), set()), "nmos.peers");
    let (c, ..) = Config::parse(&config(on(json!({})), merged.clone(), set()), host).unwrap();
    assert_eq!(c.nmos_peers(), ["f-0.f:8081", "f-1.f:8081", "f-2.f:8081"]);
    let mut port0: Value = serde_json::from_str(&config(on(json!({})), merged, set())).unwrap();
    let mut lo = port0.clone();
    lo["listeners"]["nmos"]["bind"] = json!("127.0.0.1");
    refused(lo.to_string(), "listeners.nmos.bind");
    port0["listeners"]["nmos"]["port"] = json!(0);
    port0["nmos"]["advertisePort"] = json!(8081);
    refused(port0.to_string(), "nmos.peers");
    // Every flow an output names is the mixer's own.
    let mut own = members();
    own["mergedFlow"] = json!(id(6));
    let (c, ..) = Config::parse(&config(json!({}), own, set()), host).unwrap();
    let want: std::collections::BTreeSet<String> = [4, 6, 7, 8].map(id).into();
    assert_eq!(c.own_flows(), want);
    // A loopback listener with another advertised host is noted.
    let mut loopback: Value = serde_json::from_str(&config(on(json!({})), flow(), one())).unwrap();
    loopback["listeners"]["nmos"]["bind"] = json!("127.0.0.1");
    let (c, ..) = Config::parse(&loopback.to_string(), host).unwrap();
    assert!(c.notes.iter().any(|n| n.contains("nmos.advertiseHost")));
}

/// Each channel's wiring names its input a and b flows and the domain
/// the engine reads them in: the given one, or the only one there is.
#[test]
fn the_wiring_names_each_channels_flows_and_domain() {
    let id = |n: u8| format!("00000000-0000-4000-8000-00000000000{n}");
    let config = |domains: Value, st1: Value| {
        json!({
            "replica": { "name": "r1" },
            "shape": small(),
            "mxl": {
                "domains": domains,
                "inputs": { "ch1": { "flow": id(1), "altFlow": id(5), "domain": "d" },
                            "ch2": { "flow": id(2), "domain": "d" }, "st1": st1 },
                "outputs": { "out-pgm": { "flow": id(4), "domain": "d" } },
            },
        })
        .to_string()
    };
    let d = json!({ "name": "d", "path": "/dev/shm/d" });
    let e = json!({ "name": "e", "path": "/dev/shm/e" });
    let two = config(json!([d, e]), json!({ "flow": id(3), "domain": "e" }));
    let (c, ..) = Config::parse(&two, || unreachable!()).unwrap();
    let wiring = |a: u8, b: Option<u8>, domain: &str| Wiring {
        a: id(a),
        b: b.map(id),
        domain: domain.into(),
    };
    assert_eq!(
        c.wiring,
        [
            wiring(1, Some(5), "d"),
            wiring(2, None, "d"),
            wiring(3, None, "e")
        ]
    );
    let one = config(json!([d]), json!({ "flow": id(3) }));
    let (c, ..) = Config::parse(&one, || unreachable!()).unwrap();
    assert_eq!(c.wiring[2], wiring(3, None, "d"));
    // Without an engine there is nothing to wire.
    let none = json!({ "replica": { "name": "r1" }, "shape": small() }).to_string();
    assert!(
        Config::parse(&none, || unreachable!())
            .unwrap()
            .0
            .wiring
            .is_empty()
    );
}

/// The line-up tone's generators are the engine's last inputs, after
/// the channels', one per distinct width of a bus, aux or mix-minus,
/// narrowest first and as wide, with no flow; the matrix has their
/// rows. They are no channel, so they are wired to nothing.
#[test]
fn the_line_up_tones_are_the_engines_last_inputs() {
    let id = |n: u8| format!("00000000-0000-4000-8000-00000000000{n}");
    let config = json!({
        "replica": { "name": "r1" },
        "shape": small(),
        "mxl": {
            "domains": [{ "name": "d", "path": "/dev/shm/d" }],
            "inputs": { "ch1": { "flow": id(1), "altFlow": id(5) },
                        "ch2": { "flow": id(2) }, "st1": { "flow": id(3) } },
            "outputs": { "out-pgm": { "flow": id(4) } },
        },
    });
    let (c, shape, _) = Config::parse(&config.to_string(), || unreachable!()).unwrap();
    let engine = c.audio.as_ref().unwrap();
    let names: Vec<&str> = engine.inputs.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["ch1", "ch2", "st1", "/tone/1", "/tone/2"]);
    for (tone, width) in engine.inputs[3..].iter().zip([1, 2]) {
        assert!(tone.tone.is_some() && tone.flow_id.is_none() && tone.domain.is_none());
        assert_eq!(tone.channels, width, "aux1 is mono, pgm, cf and mm1 stereo");
    }
    assert_eq!(
        engine.matrix.len(),
        4 + 1 + 2,
        "the channels' legs, then the tones'"
    );
    assert_eq!(c.wiring.len(), shape.channels.len());
}

/// A channel with a direct out, of a flow or a tone, feeds the
/// matrix before its strip as well, so its direct out can take the
/// input tap; one without has no such rows. samplez adds them after
/// every input's own rows, the line-up tones' included, in input
/// order, and the matrix counts them.
#[test]
fn a_channel_with_a_direct_out_has_pre_strip_rows_after_every_input() {
    let id = |n: u8| format!("00000000-0000-4000-8000-00000000000{n}");
    let mut shape = small();
    for (out, source, width, layout) in [("do2", "ch2", 1, "mono"), ("dst", "st1", 2, "stereo")] {
        shape["outputs"].as_array_mut().unwrap().push(json!({
            "id": out, "name": out, "source": source, "width": width, "layout": layout,
        }));
    }
    let config = json!({
        "replica": { "name": "r1" },
        "shape": shape,
        "mxl": {
            "domains": [{ "name": "d", "path": "/dev/shm/d" }],
            "inputs": { "ch1": { "flow": id(1), "altFlow": id(5) },
                        "ch2": { "tone": {} }, "st1": { "flow": id(3) } },
            "outputs": { "out-pgm": { "flow": id(4) }, "do2": { "flow": id(6) },
                         "dst": { "flow": id(7) } },
        },
    });
    let (c, shape, _) = Config::parse(&config.to_string(), || unreachable!()).unwrap();
    let engine = c.audio.as_ref().unwrap();
    let rows: Vec<(&str, bool)> = (engine.inputs.iter())
        .map(|i| (i.name.as_str(), i.pre_strip_rows))
        .collect();
    assert_eq!(
        rows,
        [
            ("ch1", false),
            ("ch2", true),
            ("st1", true),
            ("/tone/1", false),
            ("/tone/2", false),
        ]
    );
    assert_eq!(engine.rows(), 4 + 1 + 2 + 1 + 2);
    assert_eq!(engine.matrix.len(), engine.rows());
    // After the channels' 4 legs and the tones' 3, ch2's then st1's.
    assert_eq!(crate::mix::pre_rows(&shape), [None, Some(7), Some(8)]);
}

/// Every channel's input, a flow's or a tone's, meters its true peak,
/// so a channel has one without a direct out, and meters itself
/// before its strip too, as `meterTap` input can change at any time;
/// the line-up tones are no channel and meter neither. Every stereo
/// output and engine meter meters its correlation, which the contract
/// has for the stereo layout alone; samplez refuses one on a mono
/// element.
#[test]
fn channels_meter_true_peak_before_their_strip_and_stereo_correlation() {
    let id = |n: u8| format!("00000000-0000-4000-8000-00000000000{n}");
    let mut shape = crate::shape::tests::surround();
    shape["outputs"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "id": "do", "name": "d", "source": "ch1", "width": 1, "layout": "mono" }));
    let config = json!({
        "replica": { "name": "r1" },
        "shape": shape,
        "mxl": {
            "domains": [{ "name": "d", "path": "/dev/shm/d" }],
            "inputs": { "ch1": { "flow": id(1), "altFlow": id(5) },
                        "ch2": { "tone": {} }, "st1": { "flow": id(3) },
                        "sur": { "flow": id(2) } },
            "outputs": { "out-51": { "flow": id(4) }, "out-lr": { "flow": id(6) },
                         "do": { "flow": id(7) } },
        },
    });
    let (c, _, _) = Config::parse(&config.to_string(), || unreachable!()).unwrap();
    let engine = c.audio.as_ref().unwrap();
    let inputs: Vec<(&str, bool, bool)> = (engine.inputs.iter())
        .map(|i| (i.name.as_str(), i.true_peak, i.pre_strip_meter))
        .collect();
    assert_eq!(
        inputs,
        [
            ("ch1", true, true),
            ("ch2", true, true),
            ("st1", true, true),
            ("sur", true, true),
            ("/tone/1", false, false),
            ("/tone/2", false, false),
            ("/tone/6", false, false),
        ]
    );
    let outputs: Vec<(&str, bool)> = (engine.outputs.iter())
        .map(|o| (o.name.as_str(), o.correlation))
        .collect();
    assert_eq!(
        outputs,
        [("out-51", false), ("out-lr", true), ("do", false)]
    );
    let meters: Vec<(&str, bool)> = (engine.meters.iter())
        .map(|m| (m.name.as_str(), m.correlation))
        .collect();
    assert_eq!(meters, [("cf", true), ("aux1", false), ("mon", true)]);
}

/// A bus's or aux's delay line is opt-in per output carrying it, in
/// ms, the same on every one: the shape gives the element's line
/// (`maxDelay`) and the engine each such output's
/// (`maxDelaySamples`). Outputs that disagree, a line on an output
/// carrying no bus or aux, or one written into the shape are
/// refused, and the budget counts each output's line once per leg
/// beside the channels'.
#[test]
fn bus_delay_lines_are_per_output_and_within_the_budget() {
    let id = |n: u8| format!("00000000-0000-4000-8000-0000000000{n:02}");
    let mut shape = small();
    shape["outputs"] = json!([
        { "id": "out-pgm", "name": "PGM", "source": "pgm", "width": 2, "layout": "stereo" },
        { "id": "out-pgm-m", "name": "PGM mono", "source": "pgm", "width": 1, "layout": "mono" },
        { "id": "out-aux", "name": "IFB", "source": "aux1", "width": 1, "layout": "mono" },
        { "id": "out-mm", "name": "N-1", "source": "mm1", "width": 2, "layout": "stereo" },
    ]);
    let config = |shape: &Value, lines: [Value; 4], engine: Value| {
        let out = |n: u8, ms: &Value| json!({ "flow": id(n), "maxDelayMs": ms });
        json!({
            "replica": { "name": "r1" },
            "shape": shape,
            "engine": engine,
            "mxl": {
                "domains": [{ "name": "d", "path": "/dev/shm/d" }],
                "inputs": { "ch1": { "flow": id(1), "altFlow": id(5), "maxDelayMs": 20 },
                            "ch2": { "flow": id(2) }, "st1": { "flow": id(3) } },
                "outputs": {
                    "out-pgm": out(10, &lines[0]), "out-pgm-m": out(11, &lines[1]),
                    "out-aux": out(12, &lines[2]), "out-mm": out(13, &lines[3]),
                },
            },
        })
        .to_string()
    };
    let none = Value::Null;
    let (c, parsed, _) = Config::parse(
        &config(
            &shape,
            [json!(100), json!(100), json!(10), none.clone()],
            json!({}),
        ),
        || unreachable!(),
    )
    .unwrap();
    let lines: Vec<Option<u32>> = (parsed.buses.iter().map(|b| b.max_delay))
        .chain(parsed.auxes.iter().map(|a| a.max_delay))
        .collect();
    assert_eq!(lines, [Some(4800), None, Some(480)]);
    let engine: Vec<u32> = (c.audio.as_ref().unwrap().outputs.iter())
        .map(|o| o.max_delay_samples)
        .collect();
    assert_eq!(engine, [4800, 4800, 480, 0]);
    // The budget: ch1 960, PGM stereo and mono 3 legs of 4800, IFB
    // 480; one sample short of it is refused, naming every share.
    let held = 960 + 3 * 4800 + 480;
    let e = Config::parse(
        &config(
            &shape,
            [json!(100), json!(100), json!(10), none.clone()],
            json!({ "delayBudgetSamples": held - 1 }),
        ),
        || unreachable!(),
    )
    .unwrap_err();
    assert!(
        e.contains("ch1 (960), out-pgm (9600), out-pgm-m (4800), out-aux (480)"),
        "{e}"
    );
    Config::parse(
        &config(
            &shape,
            [json!(100), json!(100), json!(10), none.clone()],
            json!({ "delayBudgetSamples": held }),
        ),
        || unreachable!(),
    )
    .unwrap();
    // The outputs of one bus disagree, one of them without a line
    // included.
    for mono in [json!(50), none.clone()] {
        let e = Config::parse(
            &config(
                &shape,
                [json!(100), mono, none.clone(), none.clone()],
                json!({}),
            ),
            || unreachable!(),
        )
        .unwrap_err();
        assert!(e.contains("mxl.outputs.out-pgm-m.maxDelayMs"), "{e}");
        assert!(e.contains("takes one line"), "{e}");
    }
    let e = Config::parse(
        &config(
            &shape,
            [none.clone(), none.clone(), none.clone(), json!(10)],
            json!({}),
        ),
        || unreachable!(),
    )
    .unwrap_err();
    assert!(
        e.contains("mxl.outputs.out-mm.maxDelayMs: the output carries mm1"),
        "{e}"
    );
    let mut written = shape.clone();
    written["auxes"][0]["maxDelay"] = json!(480);
    let e = Config::parse(
        &config(
            &written,
            [none.clone(), none.clone(), none.clone(), none],
            json!({}),
        ),
        || unreachable!(),
    )
    .unwrap_err();
    assert!(e.contains("aux1 maxDelay: set by mxl.outputs"), "{e}");
}

/// The engine gives an output a strip exactly when it carries a bus,
/// an aux or a mix-minus output, and a limiter exactly when it
/// carries a bus or an aux.
#[test]
fn outputs_carrying_a_bus_aux_or_mix_minus_have_a_strip() {
    let id = |n: u8| format!("00000000-0000-4000-8000-00000000000{n}");
    let mut shape = small();
    shape["outputs"] = json!([
        { "id": "out-pgm", "name": "PGM", "source": "pgm", "width": 2, "layout": "stereo" },
        { "id": "out-aux", "name": "IFB", "source": "aux1", "width": 1, "layout": "mono" },
        { "id": "out-mm", "name": "N-1", "source": "mm1", "width": 2, "layout": "stereo" },
        { "id": "out-mon", "name": "Mon", "source": "mon", "width": 2, "layout": "stereo" },
        { "id": "out-st1", "name": "Music", "source": "st1", "width": 2, "layout": "stereo" },
    ]);
    let config = json!({
        "replica": { "name": "r1" },
        "shape": shape,
        "mxl": {
            "domains": [{ "name": "d", "path": "/dev/shm/d" }],
            "inputs": { "ch1": { "flow": id(1), "altFlow": id(2) },
                        "ch2": { "flow": id(3) }, "st1": { "flow": id(4) } },
            "outputs": { "out-pgm": { "flow": id(5) }, "out-aux": { "flow": id(6) },
                         "out-mm": { "flow": id(7) }, "out-mon": { "flow": id(8) },
                         "out-st1": { "flow": id(9) } },
        },
    });
    let (c, _, _) = Config::parse(&config.to_string(), || unreachable!()).unwrap();
    let engine = c.audio.as_ref().unwrap();
    let flags: Vec<(&str, bool, bool)> = (engine.outputs.iter())
        .map(|o| (o.name.as_str(), o.strip, o.limiter))
        .collect();
    assert_eq!(
        flags,
        [
            ("out-pgm", true, true),
            ("out-aux", true, true),
            ("out-mm", true, false),
            ("out-mon", false, false),
            ("out-st1", false, false),
        ]
    );
}

/// `replica.maxApplyLeadMs` is 50 unless set, and is taken from 30 to
/// 120 alone.
#[test]
fn the_apply_lead_cap_is_50_ms_and_bounded() {
    let config = |replica: Value| json!({ "replica": replica, "shape": small() }).to_string();
    let lead = |replica: Value| {
        Config::parse(&config(replica), || unreachable!()).map(|c| c.0.replica.max_apply_lead_ms)
    };
    assert_eq!(lead(json!({ "name": "r1" })), Ok(50));
    for ms in [30, 80, 120] {
        assert_eq!(lead(json!({ "name": "r1", "maxApplyLeadMs": ms })), Ok(ms));
    }
    for ms in [29, 121] {
        let e = lead(json!({ "name": "r1", "maxApplyLeadMs": ms })).unwrap_err();
        assert!(e.starts_with("replica.maxApplyLeadMs"), "{e}");
    }
}

#[test]
fn the_configuration_errors_read_as_they_always_have() {
    let args = |a: &[&str]| {
        command(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        args(&["--config", "c", "--oidc-nope", "x"]),
        r#"unknown argument "--oidc-nope""#
    );
    assert_eq!(
        args(&["--config", "c", "--oidc-client-id"]),
        "--oidc-client-id needs a value"
    );
    assert_eq!(
        args(&["--oidc-client-id", "x"]),
        "--config <file> is required"
    );
    let missing = std::path::Path::new("/nonexistent/faderz.json");
    assert_eq!(
        Config::load(missing, &[]).unwrap_err().to_string(),
        "/nonexistent/faderz.json: No such file or directory (os error 2)"
    );
    let flag = [("--oidc-scopes".to_owned(), "a".to_owned())];
    assert_eq!(
        apply(&mut json!({ "auth": 5 }), &flag, |_| None)
            .unwrap_err()
            .to_string(),
        "auth.oidc.scopes: not an object"
    );
    let syntax = serde_json::from_str::<Value>("{").unwrap_err().to_string();
    assert_eq!(
        Config::parse("{", || unreachable!())
            .unwrap_err()
            .to_string(),
        syntax
    );
    let shape = serde_json::from_value::<Config>(json!([]))
        .unwrap_err()
        .to_string();
    assert_eq!(
        Config::parse("[]", || unreachable!())
            .unwrap_err()
            .to_string(),
        shape
    );
}
