//! The `/v1/hubs/{hub_id}/intercom` routes: announce-only text spoken aloud on
//! a hub's speakers. The types are in [`crate::intercom`]. Not a certified
//! safety system.

use super::*;
use crate::intercom::{
    announce_body, idempotency_key, speaker_update_body, valid_id, AnnounceOptions, Announcement,
    AnnouncementList, AnnouncementListOptions, IntercomRepeat, IntercomSettings, IntercomSpeaker,
    IntercomSpeakerList, IntercomSpeakerUpdate,
};

/// The escaped path under a hub's intercom routes. An invalid id (blank, a dot
/// segment, or holding `/`, `?`, `#`, a backslash or a control character) is
/// refused before anything is sent.
fn intercom_path(hub_id: &str, route: &str) -> Result<String> {
    valid_id("hub_id", hub_id)?;
    Ok(format!(
        "/v1/hubs/{}/intercom{route}",
        urlencoding::encode(hub_id)
    ))
}

fn segment(name: &str, value: &str) -> Result<String> {
    valid_id(name, value)?;
    Ok(urlencoding::encode(value).into_owned())
}

impl ControlPlane {
    /// A hub's intercom switch, and whether it can run
    /// (`GET /v1/hubs/{id}/intercom/settings`, `intercom:read`).
    /// `unavailable_reason` is `platform` (the platform's own switch is off:
    /// every hub reads it, and it wins over the others), `plan`, `kid_hub` or
    /// `public_hub`; `enabled` is true only when the hub's switch is on and
    /// there is no reason.
    pub async fn get_intercom_settings(&self, hub_id: &str) -> Result<IntercomSettings> {
        let value = self
            .request(
                "GET",
                &intercom_path(hub_id, "/settings")?,
                None,
                None,
                true,
            )
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Turn a hub's intercom on or off (`PUT .../intercom/settings`,
    /// `hubs:write`: owner, manager or administrator of the hub, acting as
    /// themselves: an impersonated session is
    /// [`ApiRefusal::ImpersonationNotAllowed`](crate::ApiRefusal::ImpersonationNotAllowed)).
    /// A plan without intercom is
    /// [`ApiRefusal::Plan`](crate::ApiRefusal::Plan); a kid or public hub is
    /// [`ApiRefusal::IntercomUnavailable`](crate::ApiRefusal::IntercomUnavailable)
    /// (422, [`unavailable_reason`](crate::ThalovantError::unavailable_reason));
    /// a locked hub is
    /// [`ApiRefusal::HubLocked`](crate::ApiRefusal::HubLocked). While the
    /// platform switch is off, even `enabled == false` is
    /// [`ApiRefusal::IntercomDisabledPlatform`](crate::ApiRefusal::IntercomDisabledPlatform)
    /// (503: nothing was changed), and a platform that is not configured is
    /// [`ApiRefusal::IntercomNotConfigured`](crate::ApiRefusal::IntercomNotConfigured).
    pub async fn set_intercom_settings(
        &self,
        hub_id: &str,
        enabled: bool,
    ) -> Result<IntercomSettings> {
        let path = intercom_path(hub_id, "/settings")?;
        let value = self
            .request(
                "PUT",
                &path,
                Some(json!({ "enabled": enabled })),
                None,
                true,
            )
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Every connection of the hub as a speaker, with presence, policy, `kind`
    /// and `can_play` (`GET .../intercom/speakers`, `intercom:read`). The
    /// answer's `policy_synced` is `Some(false)` while the hub may hold an
    /// out-of-date consent map; `unavailable_reason` is the hub's (`platform`
    /// when intercom is off platform-wide: reads still work). A connection that
    /// cannot play speech (`can_play` false) is listed so it can be greyed out.
    pub async fn list_intercom_speakers(&self, hub_id: &str) -> Result<IntercomSpeakerList> {
        let value = self
            .request(
                "GET",
                &intercom_path(hub_id, "/speakers")?,
                None,
                None,
                true,
            )
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Change a speaker's policy (`PATCH .../intercom/speakers/{client_id}`,
    /// `hubs:write`): only what `update` sets is sent, and `update` has only
    /// the four fields the API takes (`receive`, `room`, `groups`, `dnd`). A
    /// speaker is spoken to only when `receive` is true. `dnd` enabled needs
    /// quiet hours with a valid IANA `timezone` (shape-checked) and a `start`
    /// that differs from the `end`, checked before anything is sent.
    /// `receive: true` on a connection that cannot play speech is
    /// [`ApiRefusal::NotASpeaker`](crate::ApiRefusal::NotASpeaker) (422,
    /// [`not_a_speaker_kind`](crate::ThalovantError::not_a_speaker_kind)). The
    /// answer's `policy_synced` is `Some(false)` while the hub may hold an
    /// out-of-date consent map ("changes may take a moment to reach the
    /// home"). A plain member and an impersonated session are refused.
    pub async fn update_intercom_speaker(
        &self,
        hub_id: &str,
        client_id: &str,
        update: &IntercomSpeakerUpdate,
    ) -> Result<IntercomSpeaker> {
        let body = speaker_update_body(update)?;
        let route = format!("/speakers/{}", segment("client_id", client_id)?);
        let path = intercom_path(hub_id, &route)?;
        let value = self.request("PATCH", &path, Some(body), None, true).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Say `options.text` aloud on a hub's speakers
    /// (`POST .../intercom/announce`, `intercom:send`). The API keeps only a
    /// keyed hash of the text and its length. Not a certified safety system.
    ///
    /// The text is checked first with the API's own rule (see
    /// [`announcement_text`](crate::announcement_text)); targets hold at most 50
    /// names of 1 to 64 characters, and `all`, a room or a group that resolves
    /// to more than 50 speakers is
    /// [`ApiRefusal::TooManyTargets`](crate::ApiRefusal::TooManyTargets). A named
    /// connection that cannot play speech is
    /// [`ApiRefusal::NotASpeaker`](crate::ApiRefusal::NotASpeaker).
    /// **An urgent announcement (`priority` urgent, and so every `repeat`)
    /// needs the scope `intercom:urgent` in addition to `intercom:send`**:
    /// without it the answer is
    /// [`ApiRefusal::UrgentScopeRequired`](crate::ApiRefusal::UrgentScopeRequired),
    /// before anything is spent. At most 2 urgent announcements repeat on one
    /// hub at a time ([`ApiRefusal::UrgentBusy`](crate::ApiRefusal::UrgentBusy),
    /// 429), and an urgent one needs the Intercom skill on the hub
    /// ([`ApiRefusal::SkillNotInstalled`](crate::ApiRefusal::SkillNotInstalled)).
    ///
    /// A normal announcement waits up to ten seconds for the hub and returns
    /// its result. An urgent one with `repeat` returns at once as `sending`;
    /// set `wait` to poll [`get_announcement`](Self::get_announcement) until it
    /// completes, or until `wait_timeout` passes (the latest answer is
    /// returned, still `sending`).
    ///
    /// **Idempotency.** Set `idempotency_key` to a fresh unique key (a UUID)
    /// for each announcement you mean to send, and reuse it only to repeat the
    /// *same* attempt after a network drop or timeout. The key is scoped to the
    /// hub and lasts 24 hours: the same key replays the stored announcement
    /// (the response header `Idempotency-Replayed: true`, which the SDK reads
    /// into [`Announcement::replayed`](crate::Announcement::replayed): do not
    /// tell the person it was sent anew) and **never sends again**, whatever
    /// its status; a replayed `failed` announcement stays `failed`. A key is at
    /// most 128 printable ASCII characters, and the same key with a different
    /// request is a 409. To send the same text again on purpose, use a **new**
    /// key.
    /// After [`ApiRefusal::HubUnreachable`](crate::ApiRefusal::HubUnreachable)
    /// (502 `hub_unreachable`) read the announcement first
    /// ([`ThalovantError::announcement_id`], then
    /// [`get_announcement`](Self::get_announcement)) before sending again: the
    /// hub may have spoken it. A constant key such as `"dinner"` would silence
    /// every later announcement for 24 hours.
    ///
    /// **Timeout.** The API waits up to ten seconds for the hub. This SDK sets
    /// no request timeout of its own: wrap the call in a `tokio::time::timeout`
    /// of at least twenty seconds, never less, or a spoken announcement can be
    /// reported as failed. If the call ends without an answer, the outcome is
    /// unknown: call again with the **same** key to find out.
    ///
    /// **`wait` needs `intercom:read` as well as `intercom:send`**: the polls
    /// are [`get_announcement`](Self::get_announcement). A poll can fail after
    /// the announcement was accepted (a token with only `intercom:send` is
    /// refused at the first poll; a 429; the network). The error is then
    /// [`ThalovantError::AnnouncementWaitFailed`], which carries the accepted
    /// announcement and the poll's own error ([`ThalovantError::announcement`]);
    /// the accessors for an API answer look through to it. Calling again with
    /// the same `idempotency_key` replays the stored announcement instead of
    /// speaking the text a second time; without a key it may speak it twice.
    /// `poll_interval` must be above zero, and `wait_timeout` must be a time
    /// the clock can reach; both are checked before the POST. The deadline
    /// bounds each poll, not only the sleep between polls, and no poll starts
    /// once the sleep used it.
    ///
    /// Requires the owner, a manager or an administrator acting as themselves
    /// (an impersonated session is
    /// [`ApiRefusal::ImpersonationNotAllowed`](crate::ApiRefusal::ImpersonationNotAllowed),
    /// a locked hub [`ApiRefusal::HubLocked`](crate::ApiRefusal::HubLocked)).
    /// Intercom off for the hub, the platform, or a kid or public hub is
    /// [`ApiRefusal::is_intercom_disabled`](crate::ApiRefusal::is_intercom_disabled);
    /// a hub that did not answer in 5 s is
    /// [`ApiRefusal::HubUnreachable`](crate::ApiRefusal::HubUnreachable)
    /// ([`announcement_id`](crate::ThalovantError::announcement_id)).
    /// `delivered` means the hub handed the speech to the speaker's
    /// connection: there is no playback receipt, and `failed` is not proof
    /// that nothing was spoken.
    ///
    /// Invalid input (text, targets, repeat, language tag, key, a blank id)
    /// fails locally with [`ThalovantError::Api`] before any request.
    pub async fn announce(&self, hub_id: &str, options: &AnnounceOptions) -> Result<Announcement> {
        let body = announce_body(options)?;
        let path = intercom_path(hub_id, "/announce")?;
        let headers = match options.idempotency_key.as_deref() {
            Some(key) => {
                idempotency_key(key)?;
                Some(single_header("Idempotency-Key", key)?)
            }
            None => None,
        };
        let poll = options.poll_interval.unwrap_or(Duration::from_secs(2));
        if poll.is_zero() {
            return Err(ThalovantError::Api(
                "poll_interval must be greater than zero".into(),
            ));
        }
        let repeat = options.repeat.unwrap_or(IntercomRepeat {
            times: 0,
            interval_seconds: 0,
        });
        let timeout = options.wait_timeout.unwrap_or_else(|| {
            Duration::from_secs(u64::from(repeat.times) * u64::from(repeat.interval_seconds) + 15)
        });
        if options.wait && tokio::time::Instant::now().checked_add(timeout).is_none() {
            return Err(ThalovantError::Api(
                "wait_timeout is longer than the clock can count".into(),
            ));
        }
        let (value, replayed) = self
            .request_replayed("POST", &path, Some(body), headers, true)
            .await?;
        let mut announcement: Announcement = serde_json::from_value(value)?;
        announcement.replayed = replayed;
        if !options.wait || !announcement.is_sending() {
            return Ok(announcement);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        while announcement.is_sending() {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            tokio::time::sleep_until((now + poll).min(deadline)).await;
            // With nothing left after the sleep, no read starts at all.
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            // Each read is bounded by what is left of the wait: a read the API
            // is slow to answer must not carry the wait past its deadline.
            match tokio::time::timeout_at(deadline, self.get_announcement(hub_id, &announcement.id))
                .await
            {
                Ok(Ok(latest)) => {
                    // A read is never replayed: the flag belongs to the POST.
                    announcement = Announcement { replayed, ..latest };
                }
                Ok(Err(source)) => {
                    // The announcement is accepted: its id is the caller's way back.
                    return Err(ThalovantError::AnnouncementWaitFailed {
                        announcement: Box::new(announcement),
                        source: Box::new(source),
                    });
                }
                Err(_elapsed) => break, // the latest answer, still `sending`
            }
        }
        Ok(announcement)
    }

    /// One announcement and its deliveries; poll it while `sending`
    /// (`GET .../intercom/announcements/{id}`, `intercom:read`).
    pub async fn get_announcement(
        &self,
        hub_id: &str,
        announcement_id: &str,
    ) -> Result<Announcement> {
        let route = format!(
            "/announcements/{}",
            segment("announcement_id", announcement_id)?
        );
        let value = self
            .request("GET", &intercom_path(hub_id, &route)?, None, None, true)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Stop an urgent announcement that is still repeating
    /// (`POST .../intercom/announcements/{id}/cancel`, `intercom:send`: owner or
    /// admin of the hub; a hub-scoped token is honoured). Returns the
    /// announcement with status `cancelled`.
    ///
    /// Only while it is `sending`: one that already completed, failed or was
    /// cancelled is [`ApiRefusal::NotCancellable`](crate::ApiRefusal::NotCancellable)
    /// (409 `not_cancellable`;
    /// [`announcement_status`](crate::ThalovantError::announcement_status) is
    /// its current status: read it instead of showing an error). Stopping is
    /// never refused for platform, hub or plan reasons: it works while the
    /// platform switch is off (the answer then has `hub_confirmed == Some(false)`
    /// and no hub was called), while the hub's switch is off, on a plan that
    /// lost intercom, and in an impersonated session. The API asks the hub to stop repeating; a hub
    /// that cannot confirm still leaves the record `cancelled` and the answer
    /// has `hub_confirmed == Some(false)`: then speech already queued on the
    /// hub may still play, and the hub's own stop paths (the hub switch, the
    /// platform kill switch, somebody acknowledging) remain. Not a certified
    /// safety system.
    pub async fn cancel_announcement(
        &self,
        hub_id: &str,
        announcement_id: &str,
    ) -> Result<Announcement> {
        let route = format!(
            "/announcements/{}/cancel",
            segment("announcement_id", announcement_id)?
        );
        let value = self
            .request("POST", &intercom_path(hub_id, &route)?, None, None, true)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// A hub's announcements, newest first (`GET .../intercom/announcements`,
    /// `intercom:read`); continue with `next_cursor`.
    pub async fn list_announcements(
        &self,
        hub_id: &str,
        options: &AnnouncementListOptions,
    ) -> Result<AnnouncementList> {
        let mut params = Vec::new();
        if let Some(limit) = options.limit {
            if limit == 0 {
                return Err(ThalovantError::Api(
                    "limit must be a positive integer".into(),
                ));
            }
            params.push(format!("limit={limit}"));
        }
        if let Some(cursor) = options.cursor.as_deref() {
            if cursor.trim().is_empty() {
                return Err(ThalovantError::Api("cursor must not be blank".into()));
            }
            push_query_param(&mut params, "cursor", Some(cursor));
        }
        let mut path = intercom_path(hub_id, "/announcements")?;
        if !params.is_empty() {
            path = format!("{path}?{}", params.join("&"));
        }
        let value = self.request("GET", &path, None, None, true).await?;
        Ok(serde_json::from_value(value)?)
    }
}
