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
    AnnounceOptions, Announcement, AnnouncementListOptions, AnnouncementStatus, ApiRefusal,
    ControlPlane, DeliveryStatus, IntercomDnd, IntercomPriority, IntercomQuietHours,
    IntercomRepeat, IntercomSpeakerUpdate, IntercomTargets, ThalovantError,
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

fn urgent_wait(poll: Option<Duration>, timeout: Option<Duration>) -> AnnounceOptions {
    let mut options = AnnounceOptions::new("x", IntercomTargets::All);
    options.priority = Some(IntercomPriority::Urgent);
    options.repeat = Some(IntercomRepeat {
        times: 1,
        interval_seconds: 5,
    });
    options.wait = true;
    options.poll_interval = poll;
    options.wait_timeout = timeout;
    options
}

#[tokio::test]
async fn a_bad_poll_interval_or_wait_timeout_is_refused_before_the_post() {
    let peer = serve(|_, _| answer("202 Accepted", announcement("sending", Value::Null)));
    // A zero interval would poll without a pause until a 429.
    let zero = urgent_wait(Some(Duration::ZERO), None);
    let error = peer.control.announce(HUB, &zero).await.expect_err("zero");
    assert!(
        matches!(&error, ThalovantError::Api(m) if m.contains("poll_interval")),
        "{error}"
    );
    // A wait the clock cannot reach is the Rust spelling of infinity.
    let endless = urgent_wait(None, Some(Duration::MAX));
    let error = peer
        .control
        .announce(HUB, &endless)
        .await
        .expect_err("endless");
    assert!(
        matches!(&error, ThalovantError::Api(m) if m.contains("wait_timeout")),
        "{error}"
    );
    assert!(peer.requests().is_empty(), "nothing was announced");
}

#[tokio::test]
async fn the_wait_deadline_bounds_a_stalled_status_read() {
    let peer = serve(|request, _| {
        if request.starts_with("POST") {
            answer("202 Accepted", announcement("sending", Value::Null))
        } else {
            thread::sleep(Duration::from_secs(3)); // a read that never answers in time
            ok(announcement("completed", Value::Null))
        }
    });
    let options = urgent_wait(
        Some(Duration::from_millis(10)),
        Some(Duration::from_millis(300)),
    );
    let started = std::time::Instant::now();
    let got = peer
        .control
        .announce(HUB, &options)
        .await
        .expect("the latest answer, not an error and not a hang");
    assert!(got.is_sending());
    assert_eq!(got.id, ANN);
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn no_read_starts_once_the_deadline_passed_during_the_sleep() {
    let peer = serve(|request, _| {
        if request.starts_with("POST") {
            answer("202 Accepted", announcement("sending", Value::Null))
        } else {
            ok(announcement("completed", Value::Null))
        }
    });
    let options = urgent_wait(
        Some(Duration::from_secs(30)),
        Some(Duration::from_millis(40)),
    );
    let got = peer
        .control
        .announce(HUB, &options)
        .await
        .expect("announce");
    assert!(got.is_sending()); // the sleep was cut to the deadline, and no read followed
    let requests = peer.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(line(&requests[0]).starts_with("POST"));
}

#[tokio::test]
async fn a_failed_poll_keeps_the_accepted_announcement() {
    let peer = serve(|request, _| {
        if request.starts_with("POST") {
            answer("202 Accepted", announcement("sending", Value::Null))
        } else {
            answer("403 Forbidden", json!({"detail": "Insufficient scopes"}))
        }
    });
    let mut options = urgent_wait(Some(Duration::from_millis(5)), None);
    options.idempotency_key = Some("k-1".into());
    let error = peer
        .control
        .announce(HUB, &options)
        .await
        .expect_err("a send-only token cannot poll");
    assert!(
        matches!(error, ThalovantError::AnnouncementWaitFailed { .. }),
        "{error}"
    );
    // The announcement exists: its id and status come back with the error.
    let kept = error.announcement().expect("the accepted announcement");
    assert_eq!(kept.id, ANN);
    assert!(kept.is_sending());
    // The accessors for an API answer look through to the poll's own error.
    assert_eq!(error.status_code(), Some(403));
    assert_eq!(error.api_refusal(), Some(ApiRefusal::Auth));
    assert!(!error.is_rate_limited() && !error.is_timeout() && !error.is_connection_error());
    let message = error.to_string();
    assert!(
        message.contains(ANN) && message.contains("accepted"),
        "{message}"
    );
    assert!(std::error::Error::source(&error).is_some());
    let requests = peer.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(header(&requests[0], "idempotency-key"), Some("k-1"));
    // Any other error has no announcement.
    assert!(ThalovantError::Api("x".into()).announcement().is_none());
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
async fn speakers_and_the_patch_sends_only_what_is_set() {
    let speaker = json!({
        "client_id": "c-1", "slug": "kitchen", "name": "Kitchen", "online": true, "last_seen_at": null,
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
            "c-1",
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
        format!("PATCH /v1/hubs/{HUB}/intercom/speakers/c-1 HTTP/1.1")
    );
    assert_eq!(body(&request), json!({"receive": true, "room": null}));
    peer.control
        .update_intercom_speaker(
            HUB,
            "c-1",
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
async fn settings_and_an_id_is_one_percent_encoded_path_segment() {
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
        .get_intercom_settings("a b%c")
        .await
        .expect("encoded");
    assert_eq!(
        line(&peer.last()),
        "GET /v1/hubs/a%20b%25c/intercom/settings HTTP/1.1"
    );
}

#[tokio::test]
async fn errors_are_typed() {
    let cases: Vec<(&'static str, Value, Option<ApiRefusal>, bool)> = vec![
        (
            "409 Conflict",
            json!({"code": "not_cancellable", "detail": "done"}),
            Some(ApiRefusal::NotCancellable),
            false,
        ),
        (
            "409 Conflict",
            json!({"code": "something_else", "detail": "no"}),
            None,
            false,
        ),
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
            "402 Payment Required",
            json!({"detail": "API access requires a paid plan."}),
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
        // The status is asserted on its own: a body without a `code` must not
        // let a wrong status pass.
        let expected: u16 = status
            .split_once(' ')
            .expect("status")
            .0
            .parse()
            .expect("number");
        assert_eq!(error.status_code(), Some(expected), "{status} {problem}");
        assert_eq!(error.api_refusal(), refusal, "{status} {problem}");
        assert_eq!(error.is_rate_limited(), limited, "{status} {problem}");
        if let Some(code) = problem.get("code") {
            assert_eq!(error.api_code(), code.as_str(), "{status} {problem}");
        }
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

// --- Amendment 2: cancel, replay, ids, redaction, the 502 announcement id, doc rules ---------------------

const UNSAFE_IDS: [&str; 16] = [
    ".", "..", "", "   ", "a/b", "/", "a?b", "a#b", "a\\b", "a\u{0}b", "a\nb", "a\tb", "a\u{7f}b",
    "a\u{85}b", "../x", "x/..",
];

#[tokio::test]
async fn dot_segments_and_unsafe_ids_are_refused_before_any_request() {
    let peer = serve(|_, _| ok(json!({})));
    let c = &peer.control;
    let all = || AnnounceOptions::new("hi", IntercomTargets::All);
    let receive = IntercomSpeakerUpdate {
        receive: Some(true),
        ..Default::default()
    };
    for bad in UNSAFE_IDS {
        let results = [
            (
                "get_intercom_settings",
                c.get_intercom_settings(bad).await.err(),
            ),
            (
                "set_intercom_settings",
                c.set_intercom_settings(bad, true).await.err(),
            ),
            (
                "list_intercom_speakers",
                c.list_intercom_speakers(bad).await.err(),
            ),
            (
                "update_intercom_speaker(hub)",
                c.update_intercom_speaker(bad, "c1", &receive).await.err(),
            ),
            (
                "update_intercom_speaker(client)",
                c.update_intercom_speaker(HUB, bad, &receive).await.err(),
            ),
            ("announce", c.announce(bad, &all()).await.err()),
            (
                "get_announcement(hub)",
                c.get_announcement(bad, ANN).await.err(),
            ),
            (
                "get_announcement(announcement)",
                c.get_announcement(HUB, bad).await.err(),
            ),
            (
                "cancel_announcement(hub)",
                c.cancel_announcement(bad, ANN).await.err(),
            ),
            (
                "cancel_announcement(announcement)",
                c.cancel_announcement(HUB, bad).await.err(),
            ),
            (
                "list_announcements",
                c.list_announcements(bad, &AnnouncementListOptions::default())
                    .await
                    .err(),
            ),
        ];
        for (name, error) in results {
            assert!(
                matches!(error, Some(ThalovantError::Api(_))),
                "{name}({bad:?}) = {error:?}"
            );
        }
    }
    assert!(peer.requests().is_empty(), "{:?}", peer.requests());
}

#[tokio::test]
async fn ordinary_ids_pass_the_id_check() {
    let peer = serve(|_, _| ok(announcement("completed", Value::Null)));
    for good in [
        HUB,
        ANN,
        "client-1",
        "c_1",
        "a.b",
        "...",
        "a..b",
        ".hidden",
        "name with space",
    ] {
        peer.control
            .get_announcement(HUB, good)
            .await
            .unwrap_or_else(|error| panic!("{good:?} refused: {error}"));
    }
    assert_eq!(peer.requests().len(), 9);
}

#[tokio::test]
async fn cancel_posts_to_the_cancel_route_and_reads_hub_confirmed() {
    let mut cancelled = announcement("cancelled", Value::Null);
    cancelled["priority"] = json!("urgent");
    cancelled["hub_confirmed"] = json!(true);
    let peer = serve(move |_, _| ok(cancelled.clone()));
    let got = peer
        .control
        .cancel_announcement(HUB, ANN)
        .await
        .expect("cancel");
    let request = peer.last();
    assert_eq!(
        line(&request),
        format!("POST /v1/hubs/{HUB}/intercom/announcements/{ANN}/cancel HTTP/1.1")
    );
    assert_eq!(header(&request, "authorization"), Some("Bearer tok"));
    assert!(
        request.split_once("\r\n\r\n").expect("body").1.is_empty(),
        "no body: the id in the path is all the API needs"
    );
    assert_eq!(got.status, AnnouncementStatus::Cancelled);
    assert!(!got.is_sending());
    assert_eq!(got.priority, IntercomPriority::Urgent);
    assert_eq!(got.hub_confirmed, Some(true));
    assert_eq!(
        AnnouncementStatus::from("cancelled"),
        AnnouncementStatus::Cancelled
    );
}

#[tokio::test]
async fn hub_confirmed_is_a_real_boolean_or_none() {
    for (sent, expected) in [
        (json!(false), Some(false)),
        (json!(true), Some(true)),
        (json!("yes"), None),
        (json!(1), None),
        (Value::Null, None),
    ] {
        let mut cancelled = announcement("cancelled", Value::Null);
        cancelled["hub_confirmed"] = sent.clone();
        let peer = serve(move |_, _| ok(cancelled.clone()));
        let got = peer
            .control
            .cancel_announcement(HUB, ANN)
            .await
            .expect("cancel");
        assert_eq!(got.hub_confirmed, expected, "{sent}");
    }
}

#[tokio::test]
async fn hub_confirmed_is_optional_on_every_other_answer() {
    let peer = serve(|_, _| ok(announcement("completed", Value::Null)));
    let got = peer.control.get_announcement(HUB, ANN).await.expect("get");
    assert_eq!(got.hub_confirmed, None);
    let out = serde_json::to_value(&got).expect("serialise");
    assert!(out.get("hub_confirmed").is_none(), "{out}");
    let missing: Announcement =
        serde_json::from_value(json!({"id": "a", "status": "cancelled"})).expect("minimal");
    assert_eq!(missing.hub_confirmed, None);
}

#[tokio::test]
async fn a_cancel_the_hub_could_not_confirm_is_still_a_cancelled_announcement() {
    let mut cancelled = announcement("cancelled", Value::Null);
    cancelled["hub_confirmed"] = json!(false);
    let peer = serve(move |_, _| ok(cancelled.clone()));
    let got = peer
        .control
        .cancel_announcement(HUB, ANN)
        .await
        .expect("cancel");
    assert_eq!(got.status, AnnouncementStatus::Cancelled);
    assert_eq!(got.hub_confirmed, Some(false));
}

#[tokio::test]
async fn cancel_of_a_finished_announcement_is_a_typed_409() {
    let peer = serve(|_, _| {
        answer(
            "409 Conflict",
            json!({"code": "not_cancellable", "detail": "It already completed."}),
        )
    });
    let error = peer
        .control
        .cancel_announcement(HUB, ANN)
        .await
        .expect_err("409");
    assert_eq!(error.status_code(), Some(409));
    assert_eq!(error.api_refusal(), Some(ApiRefusal::NotCancellable));
    assert_eq!(error.api_code(), Some("not_cancellable"));
    let scope = serve(|_, _| answer("403 Forbidden", json!({"detail": "Insufficient scopes"})));
    let error = scope
        .control
        .cancel_announcement(HUB, ANN)
        .await
        .expect_err("403");
    assert_eq!(error.api_refusal(), Some(ApiRefusal::Auth));
}

#[tokio::test]
async fn a_502_names_the_announcement_the_api_recorded() {
    for (problem, expected) in [
        (
            json!({"code": "hub_unreachable", "detail": "no hub", "announcement_id": ANN}),
            Some(ANN),
        ),
        (
            json!({"detail": {"code": "hub_unreachable", "detail": "no hub", "announcement_id": ANN}}),
            Some(ANN),
        ),
        (json!({"code": "hub_unreachable", "detail": "no hub"}), None),
        (
            json!({"code": "hub_unreachable", "detail": "no hub", "announcement_id": 7}),
            None,
        ),
        (
            json!({"code": "hub_unreachable", "detail": "no hub", "announcement_id": ""}),
            None,
        ),
    ] {
        let reply = problem.clone();
        let peer = serve(move |_, _| answer("502 Bad Gateway", reply.clone()));
        let mut options = AnnounceOptions::new("hello", IntercomTargets::All);
        options.idempotency_key = Some("k-1".into());
        let error = peer.control.announce(HUB, &options).await.expect_err("502");
        assert_eq!(error.status_code(), Some(502));
        assert_eq!(error.api_refusal(), Some(ApiRefusal::HubUnreachable));
        assert_eq!(error.announcement_id(), expected, "{problem}");
    }
    // Only a hub_unreachable refusal has one.
    let other = serve(|_, _| {
        answer(
            "422 Unprocessable Entity",
            json!({"code": "validation", "announcement_id": ANN}),
        )
    });
    let error = other
        .control
        .announce(HUB, &AnnounceOptions::new("hello", IntercomTargets::All))
        .await
        .expect_err("422");
    assert_eq!(error.announcement_id(), None);
}

#[tokio::test]
async fn a_replayed_failed_announcement_stays_failed_and_the_key_is_sent_each_time() {
    // The API replays the stored announcement for the same (hub, key) with
    // `Idempotency-Replayed: true` and never sends again; the SDK neither
    // fails nor retries behind the caller's back.
    let peer = serve(|_, _| Answer {
        status: "200 OK",
        body: announcement("failed", Value::Null),
        headers: vec![("Idempotency-Replayed", "true")],
    });
    let key = "11111111-1111-4111-8111-111111111111";
    let mut options = AnnounceOptions::new("Dinner is ready", IntercomTargets::All);
    options.idempotency_key = Some(key.into());
    let first = peer.control.announce(HUB, &options).await.expect("first");
    let again = peer.control.announce(HUB, &options).await.expect("again");
    assert_eq!(first.status, AnnouncementStatus::Failed);
    assert_eq!(again.status, AnnouncementStatus::Failed);
    let requests = peer.requests();
    assert_eq!(
        requests.len(),
        2,
        "nothing is retried behind the caller's back"
    );
    assert!(requests
        .iter()
        .all(|r| header(r, "idempotency-key") == Some(key)));
}

#[tokio::test]
async fn announce_options_debug_never_prints_the_text() {
    const SECRET: &str = "Take your pills at nine";
    let mut options = AnnounceOptions::new(SECRET, IntercomTargets::Rooms(vec!["kitchen".into()]));
    options.priority = Some(IntercomPriority::Urgent);
    options.repeat = Some(IntercomRepeat {
        times: 3,
        interval_seconds: 20,
    });
    options.lang = Some("en-US".into());
    options.idempotency_key = Some("k-1".into());
    let nested = vec![Some(options.clone())];
    let outputs = [
        format!("{options:?}"),
        format!("{options:#?}"),
        format!("{:?}", Box::new(options.clone())),
        format!("{nested:?}"),
        format!("{nested:#?}"),
        format!("{:?}", Some(&options)),
    ];
    for output in &outputs {
        assert!(!output.contains(SECRET), "{output}");
        assert!(!output.to_lowercase().contains("pills"), "{output}");
        assert!(output.contains("<23 chars>"), "{output}");
        assert!(
            output.contains("kitchen") && output.contains("Urgent"),
            "{output}"
        );
    }
    // The characters are counted, not the bytes.
    assert!(format!(
        "{:?}",
        AnnounceOptions::new("\u{1F600}\u{1F600}", IntercomTargets::All)
    )
    .contains("<2 chars>"));
    // Equality and cloning still see the text; only the formatting hides it.
    assert_eq!(options.clone(), options);
    // The text still goes out in the request.
    let peer = serve(|_, _| ok(announcement("completed", Value::Null)));
    peer.control
        .announce(HUB, &options)
        .await
        .expect("announce");
    assert_eq!(body(&peer.last())["text"], SECRET);
}

#[tokio::test]
async fn no_error_or_display_carries_the_text() {
    const SECRET: &str = "Take your pills at nine";
    let peer = serve(|_, _| {
        answer(
            "422 Unprocessable Entity",
            json!({"code": "validation", "detail": "bad", "input": {"text": "x"}}),
        )
    });
    let mut cases = vec![
        AnnounceOptions::new(format!("{SECRET}\u{7}"), IntercomTargets::All),
        AnnounceOptions::new(format!("{SECRET}{}", "x".repeat(300)), IntercomTargets::All),
        AnnounceOptions::new(SECRET, IntercomTargets::Rooms(vec![])),
    ];
    let mut repeat = AnnounceOptions::new(SECRET, IntercomTargets::All);
    repeat.repeat = Some(IntercomRepeat {
        times: 2,
        interval_seconds: 10,
    });
    cases.push(repeat);
    let mut key = AnnounceOptions::new(SECRET, IntercomTargets::All);
    key.idempotency_key = Some("bad\r\nkey".into());
    cases.push(key);
    cases.push(AnnounceOptions::new(SECRET, IntercomTargets::All)); // refused by the API
    for options in &cases {
        let error = peer
            .control
            .announce(HUB, options)
            .await
            .expect_err("refused");
        for text in [
            format!("{error}"),
            format!("{error:?}"),
            format!("{error:#?}"),
        ] {
            assert!(!text.contains(SECRET), "{text}");
        }
    }
}

#[test]
fn docs_use_a_fresh_key_and_do_not_over_promise() {
    // A fixed key replays the stored announcement for 24 hours and sends nothing: examples generate a fresh one.
    let mut readme = String::new();
    for name in ["README.md", "QUICK_FACTS.md", "FAQ.md"] {
        let text = std::fs::read_to_string(name).unwrap_or_else(|error| panic!("{name}: {error}"));
        for (number, line) in text.lines().enumerate() {
            let compact: String = line.split_whitespace().collect();
            assert!(
                !compact.contains("idempotency_key=Some(\""),
                "{name}:{} uses a constant idempotency key: {line}",
                number + 1
            );
        }
        let lowered = text.to_lowercase();
        for phrase in [
            "nothing listens",
            "never listens",
            "spoken once",
            "stops every announcement at once",
            "retry with the same",
            "no two-way audio and nothing",
        ] {
            assert!(!lowered.contains(phrase), "{name} says {phrase:?}");
        }
        if name == "README.md" {
            readme = lowered;
        }
    }
    for word in [
        "uuid",
        "idempotency-replayed",
        "cancel_announcement",
        "new key",
        "announcement_id",
    ] {
        assert!(readme.contains(word), "README.md does not mention {word:?}");
    }
}
