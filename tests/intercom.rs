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
use thalovant::intercom::problem_code;
use thalovant::{
    announcement_text, AnnounceOptions, Announcement, AnnouncementListOptions, AnnouncementStatus,
    ApiRefusal, ControlPlane, DeliveryStatus, IntercomDnd, IntercomPriority, IntercomQuietHours,
    IntercomRepeat, IntercomSpeakerUpdate, IntercomTargets, IntercomUnavailableReason,
    ThalovantError, DELIVERY_DETAIL_CODES, INTERCOM_READ_SCOPE, INTERCOM_SEND_SCOPE,
    INTERCOM_URGENT_SCOPE,
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

// ---------------------------------------------------------------------------
// The final API (platform security review): scope, speaker model, problem
// codes, one validation rule.
// ---------------------------------------------------------------------------

fn dnd_off() -> Value {
    json!({"enabled": false, "quiet_hours": {"start": "22:00", "end": "07:00", "timezone": null}})
}

fn speaker_row(extra: Value) -> Value {
    let mut row = json!({
        "client_id": "d8d4", "slug": "kitchen-speaker", "name": "Kitchen Speaker",
        "kind": "voice_satellite", "can_play": true, "online": false, "last_seen_at": null,
        "intercom": {"receive": true, "room": "kitchen", "groups": [], "dnd": dnd_off()}
    });
    for (key, value) in extra.as_object().expect("object") {
        row[key] = value.clone();
    }
    row
}

#[tokio::test]
async fn the_speaker_list_reads_kind_can_play_and_policy_synced() {
    let link = speaker_row(json!({
        "client_id": "996f", "slug": "home-link", "name": "Home link", "kind": "home_assistant",
        "can_play": false,
        "intercom": {"receive": false, "room": null, "groups": [], "dnd": dnd_off()}
    }));
    let reply = json!({"items": [speaker_row(json!({})), link], "unavailable_reason": null, "policy_synced": false});
    let peer = serve(move |_, _| ok(reply.clone()));
    let listed = peer
        .control
        .list_intercom_speakers(HUB)
        .await
        .expect("list");
    assert_eq!(listed.policy_synced, Some(false));
    assert_eq!(listed.unavailable_reason, None);
    let (kitchen, link) = (&listed.items[0], &listed.items[1]);
    assert_eq!(
        (
            kitchen.kind.as_str(),
            kitchen.can_play,
            kitchen.policy_synced
        ),
        ("voice_satellite", true, None)
    );
    assert_eq!(
        (link.kind.as_str(), link.can_play),
        ("home_assistant", false)
    );
    // The models serialise what they read.
    let again = serde_json::to_value(&listed).expect("serialise");
    assert_eq!(again["items"][1]["can_play"], json!(false));
    assert_eq!(again["policy_synced"], json!(false));
}

#[tokio::test]
async fn a_speaker_without_the_new_fields_keeps_the_apis_defaults() {
    let peer = serve(|_, number| {
        if number == 1 {
            ok(json!({"items": [{"client_id": "c1"}]}))
        } else {
            ok(
                json!({"items": [{"client_id": "c1", "kind": 7, "can_play": "no", "policy_synced": "yes"}]}),
            )
        }
    });
    let only = peer
        .control
        .list_intercom_speakers(HUB)
        .await
        .expect("list");
    assert_eq!(only.items[0].kind, "voice_satellite");
    assert!(only.items[0].can_play);
    assert_eq!(
        (only.policy_synced, only.unavailable_reason.clone()),
        (None, None)
    );
    // Garbage is not trusted: only an explicit false says "cannot play".
    let odd = &peer
        .control
        .list_intercom_speakers(HUB)
        .await
        .expect("odd")
        .items[0];
    assert_eq!(odd.kind, "voice_satellite");
    assert!(odd.can_play);
    assert_eq!(odd.policy_synced, None);
}

#[tokio::test]
async fn the_speaker_patch_answer_carries_its_own_policy_synced() {
    let peer = serve(|_, number| ok(speaker_row(json!({"policy_synced": number == 2}))));
    let update = IntercomSpeakerUpdate {
        receive: Some(true),
        ..Default::default()
    };
    let first = peer
        .control
        .update_intercom_speaker(HUB, "d8d4", &update)
        .await
        .expect("patch");
    assert_eq!(first.policy_synced, Some(false));
    let second = peer
        .control
        .update_intercom_speaker(HUB, "d8d4", &update)
        .await
        .expect("patch");
    assert_eq!(second.policy_synced, Some(true));
}

#[tokio::test]
async fn the_platform_reason_is_an_unavailable_reason() {
    let peer = serve(|request, _| {
        if request.contains("/speakers") {
            ok(json!({"items": [], "unavailable_reason": "platform", "policy_synced": true}))
        } else {
            ok(json!({"enabled": false, "plan_allowed": true, "unavailable_reason": "platform"}))
        }
    });
    let settings = peer
        .control
        .get_intercom_settings(HUB)
        .await
        .expect("settings");
    assert_eq!(
        settings.unavailable_reason,
        Some(IntercomUnavailableReason::Platform)
    );
    let listed = peer
        .control
        .list_intercom_speakers(HUB)
        .await
        .expect("list");
    assert_eq!(
        listed.unavailable_reason,
        Some(IntercomUnavailableReason::Platform)
    );
    assert_eq!(IntercomUnavailableReason::Platform.as_str(), "platform");
    assert!(IntercomUnavailableReason::from("platform").is_known());
}

#[test]
fn a_delivery_keeps_its_detail_code() {
    let body = json!({
        "id": ANN, "status": "failed", "priority": "normal", "created_at": null, "acked_by": null,
        "deliveries": [
            {"client_id": "c1", "slug": "k", "name": "K", "status": "failed", "detail": "hub_no_response", "at": null},
            {"client_id": "c2", "slug": "d", "name": "D", "status": "delivered", "detail": null, "at": null},
            {"client_id": "c3", "slug": "e", "name": "E", "status": "failed", "detail": "a_code_from_the_future", "at": null},
            {"client_id": "c4", "slug": "f", "name": "F", "status": "failed", "detail": 7, "at": null}
        ]
    });
    let parsed: Announcement = serde_json::from_value(body).expect("announcement");
    let details: Vec<Option<&str>> = parsed
        .deliveries
        .iter()
        .map(|d| d.detail.as_deref())
        .collect();
    assert_eq!(
        details,
        [
            Some("hub_no_response"),
            None,
            Some("a_code_from_the_future"),
            None
        ]
    );
    assert!(DELIVERY_DETAIL_CODES.contains(&"hub_no_response"));
    assert!(!DELIVERY_DETAIL_CODES.contains(&"a_code_from_the_future"));
}

#[test]
fn the_scope_names_are_constants() {
    assert_eq!(
        [
            INTERCOM_READ_SCOPE,
            INTERCOM_SEND_SCOPE,
            INTERCOM_URGENT_SCOPE
        ],
        ["intercom:read", "intercom:send", "intercom:urgent"]
    );
}

// -- announce: replayed ----------------------------------------------------

#[tokio::test]
async fn a_replayed_announcement_says_so_and_a_first_answer_does_not() {
    let peer = serve(|_, number| Answer {
        status: "200 OK",
        body: announcement("completed", Value::Null),
        headers: if number == 1 {
            vec![]
        } else {
            vec![("Idempotency-Replayed", "true")]
        },
    });
    let mut options = AnnounceOptions::new("Dinner", IntercomTargets::All);
    options.idempotency_key = Some("k-1".into());
    let first = peer.control.announce(HUB, &options).await.expect("first");
    assert!(!first.replayed);
    assert_eq!(
        serde_json::to_value(&first).expect("json")["replayed"],
        json!(false)
    );
    let again = peer.control.announce(HUB, &options).await.expect("again");
    assert!(again.replayed);
    assert_eq!(again.status, AnnouncementStatus::Completed);
}

#[tokio::test]
async fn the_replayed_header_is_read_in_any_case_and_only_when_true() {
    for (name, value, expected) in [
        ("idempotency-replayed", "TRUE", true),
        ("Idempotency-Replayed", "false", false),
        ("Idempotency-Replayed", "1", false),
    ] {
        let peer = serve(move |_, _| Answer {
            status: "200 OK",
            body: announcement("completed", Value::Null),
            headers: vec![(name, value)],
        });
        let got = peer
            .control
            .announce(HUB, &AnnounceOptions::new("Dinner", IntercomTargets::All))
            .await
            .expect("announce");
        assert_eq!(got.replayed, expected, "{name}: {value}");
    }
}

#[tokio::test]
async fn a_replayed_sending_announcement_is_202_and_stays_replayed_after_wait() {
    let peer = serve(|request, _| {
        if request.starts_with("POST") {
            Answer {
                status: "202 Accepted",
                body: announcement("sending", Value::Null),
                headers: vec![("Idempotency-Replayed", "true")],
            }
        } else {
            ok(announcement("completed", Value::Null))
        }
    });
    let mut options = urgent_wait(Some(Duration::from_millis(5)), None);
    options.idempotency_key = Some("k-2".into());
    let got = peer
        .control
        .announce(HUB, &options)
        .await
        .expect("announce");
    assert_eq!(got.status, AnnouncementStatus::Completed);
    assert!(got.replayed);
}

#[tokio::test]
async fn a_failed_wait_keeps_the_replayed_flag_on_the_accepted_announcement() {
    let peer = serve(|request, _| {
        if request.starts_with("POST") {
            Answer {
                status: "202 Accepted",
                body: announcement("sending", Value::Null),
                headers: vec![("Idempotency-Replayed", "true")],
            }
        } else {
            answer("403 Forbidden", json!({"detail": "Insufficient scopes"}))
        }
    });
    let options = urgent_wait(Some(Duration::from_millis(5)), None);
    let error = peer
        .control
        .announce(HUB, &options)
        .await
        .expect_err("poll fails");
    assert!(error.announcement().expect("accepted").replayed);
}

#[tokio::test]
async fn reads_and_cancel_are_never_replayed_and_a_body_cannot_forge_it() {
    let peer = serve(|_, _| {
        let mut body = announcement("completed", Value::Null);
        body["replayed"] = json!(true);
        Answer {
            status: "200 OK",
            body,
            headers: vec![("Idempotency-Replayed", "true")],
        }
    });
    assert!(
        !peer
            .control
            .get_announcement(HUB, ANN)
            .await
            .expect("get")
            .replayed
    );
    assert!(
        !peer
            .control
            .cancel_announcement(HUB, ANN)
            .await
            .expect("cancel")
            .replayed
    );
}

#[tokio::test]
async fn an_idempotency_key_is_at_most_128_printable_ascii_characters() {
    let peer = serve(|_, _| ok(announcement("completed", Value::Null)));
    let mut options = AnnounceOptions::new("hi", IntercomTargets::All);
    options.idempotency_key = Some("k".repeat(128));
    peer.control
        .announce(HUB, &options)
        .await
        .expect("128 characters");
    assert_eq!(
        header(&peer.last(), "idempotency-key").map(str::len),
        Some(128)
    );
    let sent = peer.requests().len();
    for bad in [
        "k".repeat(129),
        "caf\u{e9}".to_string(),
        "a\r\nb".to_string(),
        String::new(),
        "   ".to_string(),
    ] {
        options.idempotency_key = Some(bad.clone());
        let error = peer
            .control
            .announce(HUB, &options)
            .await
            .expect_err("refused");
        assert!(matches!(error, ThalovantError::Api(_)), "{bad:?}");
    }
    assert_eq!(peer.requests().len(), sent, "nothing was sent");
}

// -- the one text rule -----------------------------------------------------

fn refused_texts() -> Vec<String> {
    [
        "a<b",
        "a>b",
        "<prosody>x</prosody>",
        "line\nbreak",
        "tab\there",
        "nul\u{0} x",
        "bell\u{7} x",
        "c1 \u{85} next",
        "del\u{7f} x",
        "bidi \u{202e} override",
        "isolate \u{2066} x",
        "zero width \u{200b} space",
        "bom \u{feff} x",
        "word joiner \u{2060} x",
        "tag \u{e0041} x",
        "private \u{e000} x",
        "line \u{2028} sep",
        "para \u{2029} sep",
    ]
    .map(String::from)
    .to_vec()
}

#[tokio::test]
async fn a_text_the_api_would_refuse_is_refused_first_and_never_echoed() {
    let peer = serve(|_, _| ok(announcement("completed", Value::Null)));
    for text in refused_texts() {
        let error = peer
            .control
            .announce(
                HUB,
                &AnnounceOptions::new(text.clone(), IntercomTargets::All),
            )
            .await
            .expect_err("refused");
        assert!(matches!(error, ThalovantError::Api(_)), "{text:?}");
        for shown in [format!("{error}"), format!("{error:?}")] {
            assert!(!shown.contains(text.trim()), "{shown}");
        }
        assert!(announcement_text(&text).is_err(), "{text:?}");
    }
    assert!(peer.requests().is_empty(), "nothing was sent");
}

#[tokio::test]
async fn a_text_the_api_accepts_is_sent() {
    let peer = serve(|_, _| ok(announcement("completed", Value::Null)));
    let family = "family \u{1F468}\u{200d}\u{1F469}\u{200d}\u{1F467}";
    for text in [
        "Dinner is ready",
        "Caf\u{e9} cr\u{e8}me",
        "\u{ab} bonjour \u{bb}",
        family,
        "\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}",
        "no\u{a0}break space inside",
        "a & b, c; d: e",
        &"x".repeat(280),
    ] {
        peer.control
            .announce(HUB, &AnnounceOptions::new(text, IntercomTargets::All))
            .await
            .unwrap_or_else(|error| panic!("{text:?}: {error}"));
        assert_eq!(body(&peer.last())["text"], json!(text));
    }
}

#[test]
fn the_text_is_nfc_normalised_trimmed_and_counted_after_that() {
    let decomposed = "e\u{301}".repeat(200); // 400 code points, 200 once composed
    assert_eq!(
        announcement_text(&decomposed).expect("nfc"),
        "\u{e9}".repeat(200)
    );
    assert_eq!(
        announcement_text("  \u{a0} hi \u{3000} ").expect("trim"),
        "hi"
    );
    assert_eq!(
        announcement_text(&"\u{e9}".repeat(280))
            .expect("280")
            .chars()
            .count(),
        280
    );
    let long = announcement_text(&"\u{e9}".repeat(281)).expect_err("281");
    assert!(format!("{long}").contains("at most 280"), "{long}");
    let empty = announcement_text(" \u{a0} ").expect_err("empty");
    assert!(format!("{empty}").contains("empty"), "{empty}");
}

#[tokio::test]
async fn lang_is_a_language_tag() {
    let peer = serve(|_, _| ok(announcement("completed", Value::Null)));
    for good in ["en", "en-US", "fr-CA", "zho-Hant"] {
        let mut options = AnnounceOptions::new("hi", IntercomTargets::All);
        options.lang = Some(good.into());
        peer.control.announce(HUB, &options).await.expect(good);
    }
    let sent = peer.requests().len();
    for bad in [
        "",
        "e",
        "en_US",
        "english",
        "en-",
        "en-US-x-private-use-long",
    ] {
        let mut options = AnnounceOptions::new("hi", IntercomTargets::All);
        options.lang = Some(bad.into());
        assert!(
            matches!(
                peer.control.announce(HUB, &options).await,
                Err(ThalovantError::Api(_))
            ),
            "{bad:?}"
        );
    }
    assert_eq!(peer.requests().len(), sent);
}

// -- targets ---------------------------------------------------------------

#[tokio::test]
async fn a_target_list_holds_at_most_50_names_of_at_most_64_characters() {
    let peer = serve(|_, _| ok(announcement("completed", Value::Null)));
    type Make = fn(Vec<String>) -> IntercomTargets;
    let kinds: [(&str, Make); 3] = [
        ("speakers", IntercomTargets::Speakers),
        ("rooms", IntercomTargets::Rooms),
        ("groups", IntercomTargets::Groups),
    ];
    for (kind, make) in kinds {
        let fifty: Vec<String> = (0..50).map(|i| format!("n{i}")).collect();
        peer.control
            .announce(HUB, &AnnounceOptions::new("hi", make(fifty)))
            .await
            .expect(kind);
        peer.control
            .announce(HUB, &AnnounceOptions::new("hi", make(vec!["n".repeat(64)])))
            .await
            .expect(kind);
        let sent = peer.requests().len();
        let fifty_one: Vec<String> = (0..51).map(|i| format!("n{i}")).collect();
        let error = peer
            .control
            .announce(HUB, &AnnounceOptions::new("hi", make(fifty_one)))
            .await
            .expect_err("51 names");
        assert!(format!("{error}").contains("at most 50"), "{kind}: {error}");
        let error = peer
            .control
            .announce(HUB, &AnnounceOptions::new("hi", make(vec!["n".repeat(65)])))
            .await
            .expect_err("65 characters");
        assert!(format!("{error}").contains("at most 64"), "{kind}: {error}");
        assert_eq!(peer.requests().len(), sent, "{kind}: nothing was sent");
    }
}

// -- do not disturb --------------------------------------------------------

fn quiet(zone: Option<&str>, start: &str, end: &str, enabled: bool) -> IntercomSpeakerUpdate {
    IntercomSpeakerUpdate {
        dnd: Some(IntercomDnd {
            enabled,
            quiet_hours: Some(IntercomQuietHours {
                start: start.into(),
                end: end.into(),
                timezone: zone.map(String::from),
            }),
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn an_unusable_do_not_disturb_is_refused_before_anything_is_sent() {
    let peer = serve(|_, _| ok(speaker_row(json!({}))));
    let long_zone = format!("Europe/{}", "x".repeat(60));
    let bad = vec![
        // no window, no zone: UTC would be a guess
        IntercomSpeakerUpdate {
            dnd: Some(IntercomDnd {
                enabled: true,
                quiet_hours: None,
            }),
            ..Default::default()
        },
        quiet(None, "22:00", "07:00", true),
        // not a timezone name
        quiet(Some("Europe Paris"), "22:00", "07:00", true),
        quiet(Some("Europe/Paris "), "22:00", "07:00", true),
        quiet(Some(" Europe/Paris"), "22:00", "07:00", true),
        quiet(Some(""), "22:00", "07:00", true),
        quiet(Some(&long_zone), "22:00", "07:00", true),
        quiet(Some("/x"), "22:00", "07:00", true),
        quiet(Some("x/"), "22:00", "07:00", true),
        quiet(Some("Europe//Paris"), "22:00", "07:00", true),
        // an empty window is a mistake
        quiet(Some("Europe/Paris"), "22:00", "22:00", true),
        // any given zone is checked, even with do-not-disturb off
        quiet(Some("Europe Paris"), "22:00", "07:00", false),
        // clocks
        quiet(Some("Europe/Paris"), "25:00", "07:00", true),
        quiet(Some("Europe/Paris"), "22:00", "7:00", false),
    ];
    for update in &bad {
        assert!(
            matches!(
                peer.control
                    .update_intercom_speaker(HUB, "c1", update)
                    .await,
                Err(ThalovantError::Api(_))
            ),
            "{update:?}"
        );
    }
    assert!(peer.requests().is_empty(), "nothing was sent");
}

#[tokio::test]
async fn a_valid_do_not_disturb_is_sent() {
    let peer = serve(|_, _| ok(speaker_row(json!({}))));
    for zone in [
        "Europe/Paris",
        "America/Argentina/Buenos_Aires",
        "UTC",
        "Etc/GMT+5",
    ] {
        peer.control
            .update_intercom_speaker(HUB, "c1", &quiet(Some(zone), "22:00", "07:00", true))
            .await
            .expect(zone);
        assert_eq!(
            body(&peer.last())["dnd"]["quiet_hours"]["timezone"],
            json!(zone)
        );
    }
    // Switched off, a window and a null zone are fine.
    peer.control
        .update_intercom_speaker(HUB, "c1", &quiet(None, "21:00", "06:30", false))
        .await
        .expect("off");
    assert_eq!(
        body(&peer.last())["dnd"],
        json!({"enabled": false, "quiet_hours": {"start": "21:00", "end": "06:30", "timezone": null}})
    );
    peer.control
        .update_intercom_speaker(
            HUB,
            "c1",
            &IntercomSpeakerUpdate {
                dnd: Some(IntercomDnd::default()),
                ..Default::default()
            },
        )
        .await
        .expect("plain off");
    assert_eq!(body(&peer.last())["dnd"], json!({"enabled": false}));
}

#[test]
fn the_speaker_update_takes_only_the_documented_fields() {
    // Destructuring without `..` fails to compile when a field is added: the
    // four the API takes are all there is.
    let IntercomSpeakerUpdate {
        receive,
        room,
        groups,
        dnd,
    } = IntercomSpeakerUpdate::default();
    assert_eq!((receive, room, groups, dnd), (None, None, None, None));
}

// -- the new problem codes -------------------------------------------------

struct Case {
    status: &'static str,
    code: &'static str,
    refusal: ApiRefusal,
    extra: Value,
}

fn cases() -> Vec<Case> {
    let case = |status, code, refusal, extra| Case {
        status,
        code,
        refusal,
        extra,
    };
    vec![
        case(
            "403 Forbidden",
            "urgent_scope_required",
            ApiRefusal::UrgentScopeRequired,
            json!({}),
        ),
        case(
            "429 Too Many Requests",
            "intercom_urgent_busy",
            ApiRefusal::UrgentBusy,
            json!({"scope": "hub", "retry_after_seconds": 30}),
        ),
        case(
            "409 Conflict",
            "skill_not_installed",
            ApiRefusal::SkillNotInstalled,
            json!({}),
        ),
        case(
            "422 Unprocessable Entity",
            "not_a_speaker",
            ApiRefusal::NotASpeaker,
            json!({"kind": "home_assistant"}),
        ),
        case(
            "422 Unprocessable Entity",
            "not_a_speaker",
            ApiRefusal::NotASpeaker,
            json!({"not_speakers": ["home-link", "other"]}),
        ),
        case(
            "422 Unprocessable Entity",
            "reserved_name",
            ApiRefusal::ReservedName,
            json!({}),
        ),
        case(
            "403 Forbidden",
            "hub_locked",
            ApiRefusal::HubLocked,
            json!({}),
        ),
        case(
            "403 Forbidden",
            "impersonation_not_allowed",
            ApiRefusal::ImpersonationNotAllowed,
            json!({}),
        ),
        case(
            "422 Unprocessable Entity",
            "too_many_targets",
            ApiRefusal::TooManyTargets,
            json!({"matched": 51}),
        ),
        case(
            "422 Unprocessable Entity",
            "intercom_unavailable",
            ApiRefusal::IntercomUnavailable,
            json!({"reason": "kid_hub"}),
        ),
        case(
            "503 Service Unavailable",
            "intercom_disabled_platform",
            ApiRefusal::IntercomDisabledPlatform,
            json!({}),
        ),
        case(
            "503 Service Unavailable",
            "intercom_not_configured",
            ApiRefusal::IntercomNotConfigured,
            json!({}),
        ),
        case(
            "403 Forbidden",
            "managed_client",
            ApiRefusal::ManagedClient,
            json!({"refused_client_refs": ["client-1"]}),
        ),
        case(
            "409 Conflict",
            "not_cancellable",
            ApiRefusal::NotCancellable,
            json!({"announcement_status": "cancelled"}),
        ),
        case(
            "502 Bad Gateway",
            "hub_unreachable",
            ApiRefusal::HubUnreachable,
            json!({"announcement_id": ANN}),
        ),
    ]
}

/// Every accessor of the final API's problems, as one comparable row.
fn members(error: &ThalovantError) -> Value {
    json!({
        "announcement_id": error.announcement_id(),
        "announcement_status": error.announcement_status(),
        "unavailable_reason": error.unavailable_reason(),
        "urgent_busy_scope": error.urgent_busy_scope(),
        "not_a_speaker_kind": error.not_a_speaker_kind(),
        "not_speakers": error.not_speakers(),
        "matched_speakers": error.matched_speakers(),
        "refused_client_refs": error.refused_client_refs(),
    })
}

fn no_members() -> Value {
    json!({
        "announcement_id": null, "announcement_status": null, "unavailable_reason": null,
        "urgent_busy_scope": null, "not_a_speaker_kind": null, "not_speakers": [],
        "matched_speakers": null, "refused_client_refs": []
    })
}

fn expected_members(case: &Case) -> Value {
    let mut row = no_members();
    for (member, key) in [
        ("announcement_id", "announcement_id"),
        ("announcement_status", "announcement_status"),
        ("unavailable_reason", "reason"),
        ("urgent_busy_scope", "scope"),
        ("not_a_speaker_kind", "kind"),
        ("not_speakers", "not_speakers"),
        ("matched_speakers", "matched"),
        ("refused_client_refs", "refused_client_refs"),
    ] {
        if let Some(value) = case.extra.get(key) {
            row[member] = value.clone();
        }
    }
    row
}

#[tokio::test]
async fn each_new_problem_code_is_its_own_refusal_kind_and_reads_its_members() {
    for case in cases() {
        for nested in [false, true] {
            let mut problem =
                json!({"status": 0, "code": case.code, "message": "A sentence from the API."});
            for (key, value) in case.extra.as_object().expect("object") {
                problem[key] = value.clone();
            }
            // FastAPI's envelope: the same members inside an object `detail`.
            let sent = if nested {
                let mut inner = problem.clone();
                inner.as_object_mut().expect("object").remove("status");
                json!({"detail": inner})
            } else {
                problem
            };
            let status = case.status;
            let reply = sent.clone();
            let peer = serve(move |_, _| answer(status, reply.clone()));
            let error = peer
                .control
                .announce(HUB, &AnnounceOptions::new("Dinner", IntercomTargets::All))
                .await
                .expect_err("a refusal");
            let expected_status: u16 = status
                .split_once(' ')
                .expect("status")
                .0
                .parse()
                .expect("number");
            assert_eq!(error.status_code(), Some(expected_status), "{}", case.code);
            assert_eq!(
                error.api_refusal(),
                Some(case.refusal),
                "{} nested={nested}",
                case.code
            );
            assert_eq!(error.api_code(), Some(case.code), "{}", case.code);
            assert_eq!(
                members(&error),
                expected_members(&case),
                "{} nested={nested}",
                case.code
            );
        }
    }
}

#[test]
fn the_problem_codes_are_constants_and_each_has_a_refusal_kind() {
    let listed: std::collections::BTreeSet<&str> = cases().iter().map(|c| c.code).collect();
    let constants: std::collections::BTreeSet<&str> = [
        problem_code::URGENT_SCOPE_REQUIRED,
        problem_code::URGENT_BUSY,
        problem_code::SKILL_NOT_INSTALLED,
        problem_code::NOT_A_SPEAKER,
        problem_code::RESERVED_NAME,
        problem_code::HUB_LOCKED,
        problem_code::IMPERSONATION_NOT_ALLOWED,
        problem_code::TOO_MANY_TARGETS,
        problem_code::UNAVAILABLE,
        problem_code::DISABLED_PLATFORM,
        problem_code::NOT_CONFIGURED,
        problem_code::MANAGED_CLIENT,
        problem_code::NOT_CANCELLABLE,
        problem_code::HUB_UNREACHABLE,
    ]
    .into_iter()
    .collect();
    assert_eq!(listed, constants);
    assert_eq!(problem_code::DISABLED, "intercom_disabled");
    assert_eq!(problem_code::URGENT_SCOPE_REQUIRED, "urgent_scope_required");
}

#[test]
fn the_refusal_kinds_keep_their_relationships() {
    // Sign in again with the scope.
    assert!(ApiRefusal::Auth.is_auth() && ApiRefusal::UrgentScopeRequired.is_auth());
    assert!(!ApiRefusal::Plan.is_auth());
    // Off for the hub, or for stronger reasons.
    for kind in [
        ApiRefusal::IntercomDisabled,
        ApiRefusal::IntercomUnavailable,
        ApiRefusal::IntercomDisabledPlatform,
    ] {
        assert!(kind.is_intercom_disabled(), "{kind:?}");
    }
    assert!(!ApiRefusal::IntercomNotConfigured.is_intercom_disabled());
    assert!(!ApiRefusal::HubLocked.is_intercom_disabled());
}

#[tokio::test]
async fn the_urgent_busy_refusal_is_a_429_with_its_retry_after() {
    let peer = serve(|_, _| Answer {
        status: "429 Too Many Requests",
        body: json!({"status": 429, "code": "intercom_urgent_busy", "message": "busy", "scope": "platform", "retry_after_seconds": 30}),
        headers: vec![("Retry-After", "30")],
    });
    let mut options = AnnounceOptions::new("Wake", IntercomTargets::All);
    options.priority = Some(IntercomPriority::Urgent);
    options.repeat = Some(IntercomRepeat {
        times: 5,
        interval_seconds: 30,
    });
    let error = peer
        .control
        .announce(HUB, &options)
        .await
        .expect_err("busy");
    assert!(error.is_rate_limited());
    assert_eq!(error.api_refusal(), Some(ApiRefusal::UrgentBusy));
    assert_eq!(error.urgent_busy_scope(), Some("platform"));
    assert_eq!(
        error
            .api_problem()
            .and_then(|p| p.get("retry_after_seconds"))
            .and_then(Value::as_u64),
        Some(30)
    );
}

#[tokio::test]
async fn a_not_a_speaker_refusal_on_the_patch() {
    let peer = serve(|_, _| {
        answer(
            "422 Unprocessable Entity",
            json!({"status": 422, "code": "not_a_speaker", "message": "cannot play", "kind": "home_assistant"}),
        )
    });
    let update = IntercomSpeakerUpdate {
        receive: Some(true),
        ..Default::default()
    };
    let error = peer
        .control
        .update_intercom_speaker(HUB, "996f", &update)
        .await
        .expect_err("refused");
    assert_eq!(error.api_refusal(), Some(ApiRefusal::NotASpeaker));
    assert_eq!(error.not_a_speaker_kind(), Some("home_assistant"));
    assert!(error.not_speakers().is_empty());
}

#[tokio::test]
async fn a_problem_without_its_extra_members_has_none_of_them() {
    let peer = serve(|_, _| {
        answer(
            "422 Unprocessable Entity",
            json!({"code": "too_many_targets", "message": "too many", "matched": "51"}),
        )
    });
    let error = peer
        .control
        .announce(HUB, &AnnounceOptions::new("hi", IntercomTargets::All))
        .await
        .expect_err("refused");
    assert_eq!(error.matched_speakers(), None, "a string is not a count");
    // The accessors answer only for their own refusal.
    let other = serve(|_, _| {
        answer(
            "422 Unprocessable Entity",
            json!({"code": "validation", "matched": 5, "reason": "kid_hub", "scope": "hub", "kind": "x",
                   "announcement_status": "done", "not_speakers": ["a"], "refused_client_refs": ["b"]}),
        )
    });
    let error = other
        .control
        .announce(HUB, &AnnounceOptions::new("hi", IntercomTargets::All))
        .await
        .expect_err("refused");
    assert_eq!(members(&error), no_members());
}

#[tokio::test]
async fn a_plain_403_insufficient_scopes_is_still_auth_and_not_the_urgent_scope() {
    let peer = serve(|_, _| answer("403 Forbidden", json!({"detail": "Insufficient scopes"})));
    let error = peer
        .control
        .announce(HUB, &AnnounceOptions::new("hi", IntercomTargets::All))
        .await
        .expect_err("refused");
    assert_eq!(error.api_refusal(), Some(ApiRefusal::Auth));
    assert!(error.api_refusal().expect("kind").is_auth());
}

#[tokio::test]
async fn the_cancel_409_names_the_current_status() {
    let peer = serve(|_, _| {
        answer(
            "409 Conflict",
            json!({"status": 409, "code": "not_cancellable", "message": "Only an announcement that is still sending can be cancelled.",
                   "announcement_status": "completed"}),
        )
    });
    let error = peer
        .control
        .cancel_announcement(HUB, ANN)
        .await
        .expect_err("409");
    assert_eq!(error.api_refusal(), Some(ApiRefusal::NotCancellable));
    assert_eq!(error.announcement_status(), Some("completed"));
}

#[test]
fn the_docs_name_the_urgent_scope_and_the_new_errors() {
    let readme = std::fs::read_to_string("README.md").expect("README.md");
    for word in [
        "intercom:urgent",
        "policy_synced",
        "can_play",
        "replayed",
        "NotASpeaker",
        "TooManyTargets",
        "IntercomDisabledPlatform",
        "UrgentScopeRequired",
        "not_cancellable",
    ] {
        assert!(readme.contains(word), "README.md does not mention {word:?}");
    }
}
