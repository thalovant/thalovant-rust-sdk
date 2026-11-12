//! The `/v1/hubs/{hub_id}/intercom` routes: announce-only text spoken aloud on
//! a hub's speakers. The types are in [`crate::intercom`]. Not a certified
//! safety system.

use super::*;
use crate::intercom::{
    announce_body, speaker_update_body, AnnounceOptions, Announcement, AnnouncementList,
    AnnouncementListOptions, IntercomRepeat, IntercomSettings, IntercomSpeaker,
    IntercomSpeakerList, IntercomSpeakerUpdate,
};

/// The escaped path under a hub's intercom routes. A blank id is refused
/// before anything is sent.
fn intercom_path(hub_id: &str, route: &str) -> Result<String> {
    if hub_id.trim().is_empty() {
        return Err(ThalovantError::Api("hub_id is required".into()));
    }
    Ok(format!(
        "/v1/hubs/{}/intercom{route}",
        urlencoding::encode(hub_id)
    ))
}

fn segment(name: &str, value: &str) -> Result<String> {
    if value.trim().is_empty() {
        return Err(ThalovantError::Api(format!("{name} is required")));
    }
    Ok(urlencoding::encode(value).into_owned())
}

impl ControlPlane {
    /// A hub's intercom switch, and whether it can be turned on
    /// (`GET /v1/hubs/{id}/intercom/settings`, `intercom:read`).
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
    /// `hubs:write`: owner or admin of the hub). A plan without intercom is
    /// [`ApiRefusal::Plan`](crate::ApiRefusal::Plan); a kid or public hub is a
    /// 422.
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

    /// Every client of the hub as a speaker, with presence and policy
    /// (`GET .../intercom/speakers`, `intercom:read`).
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
    /// `hubs:write`): only what `update` sets is sent. A speaker is spoken to
    /// only when `receive` is true.
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
    /// (`POST .../intercom/announce`, `intercom:send`). The platform keeps only
    /// the text's hash and length. Not a certified safety system.
    ///
    /// A normal announcement waits up to ten seconds for the hub and returns
    /// its result. An urgent one with `repeat` returns at once as `sending`;
    /// set `wait` to poll [`get_announcement`](Self::get_announcement) until it
    /// completes, or until `wait_timeout` passes (the latest answer is
    /// returned, still `sending`). Keep an `idempotency_key` to retry after an
    /// uncertain outcome without speaking twice.
    ///
    /// **`wait` needs `intercom:read` as well as `intercom:send`**: the polls
    /// are [`get_announcement`](Self::get_announcement). A poll can fail after
    /// the announcement was accepted (a token with only `intercom:send` is
    /// refused at the first poll; a 429; the network). The error is then
    /// [`ThalovantError::AnnouncementWaitFailed`], which carries the accepted
    /// announcement and the poll's own error ([`ThalovantError::announcement`]);
    /// the accessors for an API answer look through to it. A retry still needs
    /// the same `idempotency_key`, or it may speak the text a second time.
    /// `poll_interval` must be above zero, and `wait_timeout` must be a time
    /// the clock can reach; both are checked before the POST. The deadline
    /// bounds each poll, not only the sleep between polls, and no poll starts
    /// once the sleep used it.
    ///
    /// Invalid input (text, targets, repeat, a blank id) fails locally with
    /// [`ThalovantError::Api`] before any request.
    pub async fn announce(&self, hub_id: &str, options: &AnnounceOptions) -> Result<Announcement> {
        let body = announce_body(options)?;
        let path = intercom_path(hub_id, "/announce")?;
        let headers = match options.idempotency_key.as_deref() {
            Some(key) if key.trim().is_empty() => {
                return Err(ThalovantError::Api(
                    "idempotency_key must not be blank".into(),
                ));
            }
            Some(key) => Some(single_header("Idempotency-Key", key)?),
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
        let value = self
            .request("POST", &path, Some(body), headers, true)
            .await?;
        let mut announcement: Announcement = serde_json::from_value(value)?;
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
                Ok(Ok(latest)) => announcement = latest,
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
