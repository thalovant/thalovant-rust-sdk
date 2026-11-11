//! Intercom: the `/v1/hubs/{hub_id}/intercom` routes against a real loopback
//! HTTP peer that records every request as it arrived, so what is asserted is
//! what reached the wire: the method, the escaped path, the query, the exact
//! JSON body and the headers.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use thalovant::{
    AnnounceOptions, AnnouncementListOptions, AnnouncementStatus, ApiRefusal, ControlPlane,
    DeliveryStatus, IntercomDnd, IntercomPriority, IntercomQuietHours, IntercomRepeat,
    IntercomSpeakerUpdate, IntercomTargets, ThalovantError,
};

const HUB: &str = "0f8a7c3e-2b1d-4c5e-9a6f-1d2c3b4a5e6f";
const ANN: &str = "7a1d6c1e-6f0b-4f43-9d7e-3f4f0d9a7c11";

/// One scripted answer.
#[derive(Clone)]
struct Answer {
    status: &'static str,
    body: Value,
    headers: Vec<(&'static str, &'static str)>,
}

fn ok(body: Value) -> Answer {
    Answer {
        status: "200 OK",
        body,
        headers: vec![],
    }
}

fn answer(status: &'static str, body: Value) -> Answer {
    Answer {
        status,
        body,
        headers: vec![],
    }
}

fn announcement(status: &str, acked_by: Value) -> Value {
    json!({
        "id": ANN, "status": status, "priority": "normal", "created_at": "2026-11-11T10:00:00Z",
        "deliveries": [
            {"client_id": "c1", "slug": "kitchen", "name": "Kitchen", "status": "delivered", "at": "2026-11-11T10:00:01Z"},
            {"client_id": "c2", "slug": "den", "name": "Den", "status": "brand_new_status", "at": null}
        ],
        "acked_by": acked_by
    })
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let size = stream.read(&mut chunk).expect("read request");
        assert!(size > 0, "request closed early");
        raw.extend_from_slice(&chunk[..size]);
        if let Some(end) = raw.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let head = std::str::from_utf8(&raw[..end]).expect("UTF-8 head");
            let length = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, length)| length.trim().parse::<usize>().expect("length"))
                .unwrap_or(0);
            if raw.len() >= end + 4 + length {
                return String::from_utf8(raw).expect("UTF-8 request");
            }
        }
    }
}

/// A loopback API answering every request from `route` (given the request
/// line and the request number), recording all of them.
struct Peer {
    control: ControlPlane,
    seen: Arc<Mutex<Vec<String>>>,
}

fn serve(route: impl Fn(&str, usize) -> Answer + Send + 'static) -> Peer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let request = read_request(&mut stream);
            let number = {
                let mut log = log.lock().unwrap();
                log.push(request.clone());
                log.len()
            };
            let line = request.lines().next().unwrap_or("").to_string();
            let reply = route(&line, number);
            let body = reply.body.to_string();
            let extra: String = reply
                .headers
                .iter()
                .map(|(k, v)| format!("{k}: {v}\r\n"))
                .collect();
            let head = format!(
                "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n",
                reply.status,
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    Peer {
        control: ControlPlane::new(format!("http://{address}"), Some("tok".into())),
        seen,
    }
}

impl Peer {
    fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
    fn last(&self) -> String {
        self.requests().last().cloned().expect("a request")
    }
}

fn line(request: &str) -> &str {
    request.lines().next().expect("line")
}

fn body(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").expect("body").1).expect("JSON")
}

fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request
        .split_once("\r\n\r\n")?
        .0
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .find(|(n, _)| n.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim())
}

#[tokio::test]
async fn announce_sends_the_contract_body_and_reads_deliveries() {
    let peer = serve(|_, _| ok(announcement("completed", Value::Null)));
    let mut options = AnnounceOptions::new(
        "  Dinner is ready  ",
        IntercomTargets::Rooms(vec!["kitchen".into()]),
    );
    options.lang = Some("en-US".into());
    let got = peer
        .control
        .announce(HUB, &options)
        .await
        .expect("announce");
    let request = peer.last();
    assert_eq!(
        line(&request),
        format!("POST /v1/hubs/{HUB}/intercom/announce HTTP/1.1")
    );
    assert_eq!(
        body(&request),
        json!({"text": "Dinner is ready", "targets": {"rooms": ["kitchen"]}, "priority": "normal", "lang": "en-US"})
    );
    assert_eq!(header(&request, "authorization"), Some("Bearer tok"));
    assert_eq!(header(&request, "idempotency-key"), None);
    assert_eq!(got.status, AnnouncementStatus::Completed);
    assert!(!got.is_sending());
    assert_eq!(got.deliveries[0].status, DeliveryStatus::Delivered);
    assert_eq!(
        got.deliveries[1].status,
        DeliveryStatus::Other("brand_new_status".into())
    );
    assert_eq!(got.acked_by, None);
}

#[tokio::test]
async fn urgent_repeat_idempotency_and_wait_polls_until_completed() {
    let peer = serve(|request, number| {
        if request.starts_with("POST") {
            answer("202 Accepted", announcement("sending", Value::Null))
        } else if number < 3 {
            ok(announcement("sending", Value::Null))
        } else {
            ok(announcement("completed", json!("Den")))
        }
    });
    let mut options = AnnounceOptions::new("Wake up", IntercomTargets::All);
    options.priority = Some(IntercomPriority::Urgent);
    options.repeat = Some(IntercomRepeat {
        times: 2,
        interval_seconds: 5,
    });
    options.idempotency_key = Some("key-1".into());
    options.wait = true;
    options.poll_interval = Some(Duration::from_millis(5));
    let got = peer
        .control
        .announce(HUB, &options)
        .await
        .expect("announce");
    let requests = peer.requests();
    assert_eq!(
        body(&requests[0])["repeat"],
        json!({"times": 2, "interval_seconds": 5})
    );
    assert_eq!(body(&requests[0])["targets"], json!({"all": true}));
    assert_eq!(header(&requests[0], "idempotency-key"), Some("key-1"));
    assert_eq!(got.status, AnnouncementStatus::Completed);
    assert_eq!(got.acked_by.as_deref(), Some("Den"));
    assert_eq!(requests.len(), 3);
    assert_eq!(
        line(&requests[1]),
        format!("GET /v1/hubs/{HUB}/intercom/announcements/{ANN} HTTP/1.1")
    );
}

#[tokio::test]
async fn a_wait_that_times_out_returns_the_latest_answer() {
    let peer = serve(|_, _| ok(announcement("sending", Value::Null)));
    let mut options = AnnounceOptions::new("x", IntercomTargets::Groups(vec!["kids".into()]));
    options.priority = Some(IntercomPriority::Urgent);
    options.repeat = Some(IntercomRepeat {
        times: 1,
        interval_seconds: 5,
    });
    options.wait = true;
    options.wait_timeout = Some(Duration::from_millis(40));
    options.poll_interval = Some(Duration::from_millis(5));
    let got = peer
        .control
        .announce(HUB, &options)
        .await
        .expect("announce");
    assert!(got.is_sending());
}

#[tokio::test]
async fn get_and_list_announcements() {
    let peer = serve(|request, _| {
        if request.contains("/announcements/") {
            ok(announcement("completed", Value::Null))
        } else {
            ok(json!({"items": [announcement("completed", Value::Null)], "next_cursor": "c2"}))
        }
    });
    assert_eq!(
        peer.control
            .get_announcement(HUB, ANN)
            .await
            .expect("get")
            .id,
        ANN
    );
    let page = peer
        .control
        .list_announcements(
            HUB,
            &AnnouncementListOptions {
                limit: Some(5),
                cursor: Some("c 1".into()),
            },
        )
        .await
        .expect("list");
    assert_eq!(page.next_cursor.as_deref(), Some("c2"));
    assert_eq!(page.items.len(), 1);
    let request = peer.last();
    let target = line(&request).split(' ').nth(1).unwrap().to_string();
    assert!(
        target.starts_with(&format!("/v1/hubs/{HUB}/intercom/announcements?")),
        "{target}"
    );
    assert!(
        target.contains("limit=5") && target.contains("cursor=c%201"),
        "{target}"
    );

    let empty = serve(|_, _| ok(json!({"items": []})));
    assert_eq!(
        empty
            .control
            .list_announcements(HUB, &AnnouncementListOptions::default())
            .await
            .unwrap()
            .next_cursor,
        None
    );
}

#[tokio::test]
async fn speakers_and_the_patch_sends_only_what_is_set_and_escapes_the_id() {
    let speaker = json!({
        "client_id": "c/1", "slug": "kitchen", "name": "Kitchen", "online": true, "last_seen_at": null,
        "intercom": {"receive": true, "room": "kitchen", "groups": ["downstairs"],
                     "dnd": {"enabled": true, "quiet_hours": {"start": "22:00", "end": "07:00", "timezone": null}}}
    });
    let reply = speaker.clone();
    let peer = serve(move |request, _| {
        if request.starts_with("GET") {
            ok(json!({"items": [reply.clone()]}))
        } else {
            ok(reply.clone())
        }
    });
    let listed = peer
        .control
        .list_intercom_speakers(HUB)
        .await
        .expect("list");
    assert!(listed.items[0].online);
    assert_eq!(listed.items[0].intercom.room.as_deref(), Some("kitchen"));
    assert_eq!(
        listed.items[0].intercom.dnd.quiet_hours,
        Some(IntercomQuietHours {
            start: "22:00".into(),
            end: "07:00".into(),
            timezone: None
        })
    );
    peer.control
        .update_intercom_speaker(
            HUB,
            "c/1",
            &IntercomSpeakerUpdate {
                receive: Some(true),
                room: Some(None),
                ..Default::default()
            },
        )
        .await
        .expect("patch");
    let request = peer.last();
    assert_eq!(
        line(&request),
        format!("PATCH /v1/hubs/{HUB}/intercom/speakers/c%2F1 HTTP/1.1")
    );
    assert_eq!(body(&request), json!({"receive": true, "room": null}));
    peer.control
        .update_intercom_speaker(
            HUB,
            "c/1",
            &IntercomSpeakerUpdate {
                dnd: Some(IntercomDnd {
                    enabled: true,
                    quiet_hours: Some(IntercomQuietHours {
                        start: "22:00".into(),
                        end: "07:00".into(),
                        timezone: Some("Europe/Paris".into()),
                    }),
                }),
                groups: Some(vec![]),
                ..Default::default()
            },
        )
        .await
        .expect("patch");
    let sent = body(&peer.last());
    assert_eq!(sent["dnd"]["quiet_hours"]["timezone"], "Europe/Paris");
    assert_eq!(sent["groups"], json!([]));
}

#[tokio::test]
async fn settings_and_an_id_is_escaped_as_one_path_segment() {
    let peer = serve(|request, _| {
        if request.starts_with("PUT") {
            ok(json!({"enabled": true, "plan_allowed": true, "unavailable_reason": null}))
        } else {
            ok(json!({"enabled": false, "plan_allowed": false, "unavailable_reason": "plan"}))
        }
    });
    let got = peer.control.get_intercom_settings(HUB).await.expect("get");
    assert_eq!(
        got.unavailable_reason.as_ref().map(|r| r.as_str()),
        Some("plan")
    );
    let set = peer
        .control
        .set_intercom_settings(HUB, true)
        .await
        .expect("put");
    assert!(set.enabled && set.unavailable_reason.is_none());
    assert_eq!(body(&peer.last()), json!({"enabled": true}));
    peer.control
        .get_intercom_settings("a/b?c")
        .await
        .expect("escaped");
    assert_eq!(
        line(&peer.last()),
        "GET /v1/hubs/a%2Fb%3Fc/intercom/settings HTTP/1.1"
    );
}

#[tokio::test]
async fn errors_are_typed() {
    let cases: Vec<(&'static str, Value, Option<ApiRefusal>, bool)> = vec![
        (
            "403 Forbidden",
            json!({"code": "intercom_disabled", "detail": "off"}),
            Some(ApiRefusal::IntercomDisabled),
            false,
        ),
        (
            "422 Unprocessable Entity",
            json!({"code": "intercom_disabled", "detail": "kid hub"}),
            Some(ApiRefusal::IntercomDisabled),
            false,
        ),
        (
            "502 Bad Gateway",
            json!({"code": "hub_unreachable", "detail": "no hub"}),
            Some(ApiRefusal::HubUnreachable),
            false,
        ),
        (
            "429 Too Many Requests",
            json!({"code": "rate_limited", "detail": "slow", "retry_after_seconds": 30}),
            None,
            true,
        ),
        (
            "403 Forbidden",
            json!({"code": "plan_limit", "detail": "plan"}),
            Some(ApiRefusal::Plan),
            false,
        ),
        (
            "403 Forbidden",
            json!({"detail": "Insufficient scopes"}),
            Some(ApiRefusal::Auth),
            false,
        ),
        (
            "422 Unprocessable Entity",
            json!({"code": "validation", "detail": "bad"}),
            None,
            false,
        ),
    ];
    for (status, problem, refusal, limited) in cases {
        let reply = problem.clone();
        let peer = serve(move |_, _| answer(status, reply.clone()));
        let error = peer
            .control
            .announce(HUB, &AnnounceOptions::new("hello", IntercomTargets::All))
            .await
            .expect_err("a refusal");
        assert_eq!(error.api_refusal(), refusal, "{status} {problem}");
        assert_eq!(error.is_rate_limited(), limited, "{status} {problem}");
        assert_eq!(error.api_code(), problem["code"].as_str());
    }
}

#[tokio::test]
async fn nothing_is_sent_for_invalid_input() {
    let peer = serve(|_, _| ok(json!({})));
    let all = || IntercomTargets::All;
    let many = IntercomTargets::Speakers((0..51).map(|i| format!("s{i}")).collect());
    let mut bad: Vec<AnnounceOptions> = vec![
        AnnounceOptions::new("", all()),
        AnnounceOptions::new("   ", all()),
        AnnounceOptions::new("x".repeat(281), all()),
        AnnounceOptions::new("a\u{7}b", all()),
        AnnounceOptions::new("a\u{2028}b", all()),
        AnnounceOptions::new("hi", IntercomTargets::Rooms(vec![])),
        AnnounceOptions::new("hi", IntercomTargets::Rooms(vec![" ".into()])),
        AnnounceOptions::new("hi", many),
    ];
    let mut repeat_on_normal = AnnounceOptions::new("hi", all());
    repeat_on_normal.repeat = Some(IntercomRepeat {
        times: 2,
        interval_seconds: 10,
    });
    bad.push(repeat_on_normal);
    for (times, interval_seconds) in [(0, 10), (11, 10), (2, 4), (2, 121)] {
        let mut options = AnnounceOptions::new("hi", all());
        options.priority = Some(IntercomPriority::Urgent);
        options.repeat = Some(IntercomRepeat {
            times,
            interval_seconds,
        });
        bad.push(options);
    }
    let mut loud = AnnounceOptions::new("hi", all());
    loud.priority = Some(IntercomPriority::Other("loud".into()));
    bad.push(loud);
    let mut blank_key = AnnounceOptions::new("hi", all());
    blank_key.idempotency_key = Some("  ".into());
    bad.push(blank_key);
    for options in &bad {
        assert!(
            matches!(
                peer.control.announce(HUB, options).await,
                Err(ThalovantError::Api(_))
            ),
            "{:?}",
            options.text
        );
    }
    assert!(peer
        .control
        .announce("  ", &AnnounceOptions::new("hi", all()))
        .await
        .is_err());
    assert!(peer.control.get_announcement(HUB, "").await.is_err());
    for update in [
        IntercomSpeakerUpdate::default(),
        IntercomSpeakerUpdate {
            room: Some(Some("r".repeat(65))),
            ..Default::default()
        },
        IntercomSpeakerUpdate {
            groups: Some(vec!["g".into(); 9]),
            ..Default::default()
        },
        IntercomSpeakerUpdate {
            groups: Some(vec!["".into()]),
            ..Default::default()
        },
        IntercomSpeakerUpdate {
            dnd: Some(IntercomDnd {
                enabled: true,
                quiet_hours: Some(IntercomQuietHours {
                    start: "25:00".into(),
                    end: "07:00".into(),
                    timezone: None,
                }),
            }),
            ..Default::default()
        },
    ] {
        assert!(
            peer.control
                .update_intercom_speaker(HUB, "c1", &update)
                .await
                .is_err(),
            "{update:?}"
        );
    }
    assert!(peer
        .control
        .list_announcements(
            HUB,
            &AnnouncementListOptions {
                limit: Some(0),
                cursor: None
            }
        )
        .await
        .is_err());
    assert!(
        peer.requests().is_empty(),
        "no request may be sent: {:?}",
        peer.requests()
    );
}

#[tokio::test]
async fn the_text_counts_characters_not_bytes() {
    let peer = serve(|_, _| ok(announcement("completed", Value::Null)));
    let text = "\u{1F600}".repeat(280);
    peer.control
        .announce(HUB, &AnnounceOptions::new(text, IntercomTargets::All))
        .await
        .expect("280 characters");
}

#[tokio::test]
async fn malformed_answers_are_errors() {
    let peer = serve(|_, _| ok(json!({"status": "completed"})));
    assert!(peer
        .control
        .announce(HUB, &AnnounceOptions::new("hi", IntercomTargets::All))
        .await
        .is_err());
    let listing = serve(|_, _| ok(json!({"nope": 1})));
    assert!(listing
        .control
        .list_announcements(HUB, &AnnouncementListOptions::default())
        .await
        .is_err());
}
