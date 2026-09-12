use super::*;

/// Optional polling after an accepted shared-runtime skill mutation.
#[derive(Clone, Debug)]
pub struct HubSkillWaitOptions {
    pub wait: bool,
    pub timeout: Duration,
    pub poll_interval: Duration,
}
impl Default for HubSkillWaitOptions {
    fn default() -> Self {
        Self {
            wait: false,
            timeout: Duration::from_secs(120),
            poll_interval: Duration::from_secs(2),
        }
    }
}
impl HubSkillWaitOptions {
    fn validate(&self) -> Result<()> {
        if self.timeout.is_zero()
            || self.poll_interval.is_zero()
            || tokio::time::Instant::now()
                .checked_add(self.timeout)
                .is_none()
        {
            return Err(ThalovantError::Api(
                "hub skill wait durations must be positive".into(),
            ));
        }
        Ok(())
    }
}
fn skills_path(hub_id: &str) -> String {
    format!("/v1/hubs/{}/skills", urlencoding::encode(hub_id))
}
impl ControlPlane {
    /// Read skills from the shared runtime behind this hub (hubs:inspect).
    pub async fn list_hub_skills(&self, hub_id: &str) -> Result<Value> {
        self.request("GET", &skills_path(hub_id), None, None, true)
            .await
    }
    /// Read newest-first shared-runtime skill history; limit is 1–200.
    pub async fn list_hub_skill_history(&self, hub_id: &str, limit: u16) -> Result<Value> {
        if !(1..=200).contains(&limit) {
            return Err(ThalovantError::Api("limit must be from 1 to 200".into()));
        }
        self.request(
            "GET",
            &format!("{}/history?limit={limit}", skills_path(hub_id)),
            None,
            None,
            true,
        )
        .await
    }
    /// Install on the hub's shared runtime, affecting every hub it serves.
    /// Version is "latest" or exact; requires hubs:write and a paid plan.
    pub async fn install_hub_skill(
        &self,
        hub_id: &str,
        skill: &str,
        version: &str,
        opts: HubSkillWaitOptions,
    ) -> Result<Value> {
        self.change_hub_skill(
            "POST",
            &skills_path(hub_id),
            Some(json!({"skill":skill,"version":version})),
            opts,
        )
        .await
    }
    /// Move a shared-runtime skill to an exact version or "latest".
    pub async fn update_hub_skill(
        &self,
        hub_id: &str,
        skill: &str,
        version: &str,
        opts: HubSkillWaitOptions,
    ) -> Result<Value> {
        self.change_hub_skill(
            "PATCH",
            &format!("{}/{}", skills_path(hub_id), urlencoding::encode(skill)),
            Some(json!({"version":version})),
            opts,
        )
        .await
    }
    /// Remove the shared-runtime skill, affecting every hub it serves.
    pub async fn remove_hub_skill(
        &self,
        hub_id: &str,
        skill: &str,
        opts: HubSkillWaitOptions,
    ) -> Result<Value> {
        self.change_hub_skill(
            "DELETE",
            &format!("{}/{}", skills_path(hub_id), urlencoding::encode(skill)),
            None,
            opts,
        )
        .await
    }
    async fn change_hub_skill(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        opts: HubSkillWaitOptions,
    ) -> Result<Value> {
        opts.validate()?;
        let accepted = self.request(method, path, body, None, true).await?;
        if !opts.wait {
            return Ok(accepted);
        }
        self.wait_for_hub_skill_operation(&accepted, opts).await
    }
    /// Resume an accepted operation without repeating the mutation.
    /// For cancellation-sensitive callers, submit without waiting, retain the
    /// complete accepted response, then await this separately. Dropping the future stops polling.
    pub async fn wait_for_hub_skill_operation(
        &self,
        accepted: &Value,
        opts: HubSkillWaitOptions,
    ) -> Result<Value> {
        opts.validate()?;
        let id = accepted["operation_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ThalovantError::Api("missing accepted operation_id".into()))?;
        let state = if matches!(accepted["state"].as_str(), Some("removing" | "removed")) {
            "removed"
        } else {
            "installed"
        };
        let deadline = tokio::time::Instant::now()
            .checked_add(opts.timeout)
            .ok_or_else(|| ThalovantError::Api("hub skill timeout is too large".into()))?;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(ThalovantError::Timeout(format!("accepted operation {id}")));
            }
            let operation = tokio::time::timeout_at(deadline, self.get_operation(id))
                .await
                .map_err(|_| ThalovantError::Timeout(format!("accepted operation {id}")))?
                .map_err(|_| {
                ThalovantError::Api(format!(
                    "could not read accepted operation {id}; inspect by ID or resume with the complete accepted response"
                ))
            })?;
            match operation.status {
                OperationStatus::Ready => {
                    let mut result = accepted.clone();
                    result["state"] = json!(state);
                    result["operation"] = serde_json::to_value(&operation)?;
                    return Ok(result);
                }
                OperationStatus::Failed | OperationStatus::TimedOut => {
                    return Err(ThalovantError::Api(format!(
                        "accepted operation {id} failed; inspect get_operation for details"
                    )))
                }
                _ => {}
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(opts.poll_interval.min(remaining)).await;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn server(
        replies: Vec<(&'static str, Value)>,
    ) -> (
        ControlPlane,
        std::thread::JoinHandle<()>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = ControlPlane::new(
            format!("http://{}", listener.local_addr().unwrap()),
            Some("token".into()),
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let saved = requests.clone();
        let worker = std::thread::spawn(move || {
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    };
                    bytes.extend_from_slice(&buf[..n]);
                    if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                saved
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&bytes).to_string());
                let body = body.to_string();
                write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        });
        (api, worker, requests)
    }
    fn operation(status: &str) -> Value {
        json!({"id":"op-1","kind":"runtime_group.sync","aggregate_type":"runtime_group","status":status,"details":{},"created_at":"now","updated_at":"now","links":{}})
    }
    #[tokio::test]
    async fn requests_encode_paths_and_resume_without_replaying() {
        let accepted = json!({"operation_id":"op-1","state":"installing","skill":"s/1"});
        let (api, worker, requests) = server(vec![
            ("200 OK", json!({"data":[]})),
            (
                "200 OK",
                json!({"data":[{"kind":"event","actor_email":null}]}),
            ),
            ("202 Accepted", accepted.clone()),
            ("200 OK", operation("ready")),
            ("202 Accepted", accepted.clone()),
            ("202 Accepted", accepted.clone()),
        ]);
        api.list_hub_skills("h/1").await.unwrap();
        assert!(
            api.list_hub_skill_history("h/1", 200).await.unwrap()["data"][0]["actor_email"]
                .is_null()
        );
        let got = api
            .install_hub_skill("h/1", "s/1", "1.2.0", HubSkillWaitOptions::default())
            .await
            .unwrap();
        let done = api
            .wait_for_hub_skill_operation(&got, HubSkillWaitOptions::default())
            .await
            .unwrap();
        assert_eq!(done["state"], "installed");
        assert_eq!(got["state"], "installing");
        api.update_hub_skill("h/1", "s/1", "1.2.0", HubSkillWaitOptions::default())
            .await
            .unwrap();
        api.remove_hub_skill("h/1", "s/1", HubSkillWaitOptions::default())
            .await
            .unwrap();
        worker.join().unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
        assert!(requests[0].starts_with("GET /v1/hubs/h%2F1/skills "));
        assert!(requests[1].contains("/history?limit=200 "));
        assert!(requests[4].starts_with("PATCH /v1/hubs/h%2F1/skills/s%2F1 "));
        assert!(requests[5].starts_with("DELETE /v1/hubs/h%2F1/skills/s%2F1 "));
    }
    #[tokio::test]
    async fn polling_failures_keep_id_and_timeout_does_not_start_another_read() {
        for status in ["failed", "timed_out", "applied", "http-error"] {
            let response = if status == "http-error" {
                ("503 Unavailable", json!({"detail":"private-data"}))
            } else {
                ("200 OK", operation(status))
            };
            let (api, worker, requests) = server(vec![
                (
                    "202 Accepted",
                    json!({"operation_id":"op-1","state":"installing"}),
                ),
                response,
            ]);
            let err = api
                .install_hub_skill(
                    "h",
                    "s",
                    "latest",
                    HubSkillWaitOptions {
                        wait: true,
                        timeout: Duration::from_millis(20),
                        poll_interval: Duration::from_secs(2),
                    },
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("op-1"));
            assert!(!err.to_string().contains("private-data"));
            worker.join().unwrap();
            assert_eq!(requests.lock().unwrap().len(), 2);
        }
    }
    #[tokio::test]
    async fn stalled_poll_is_bounded_and_keeps_the_operation_id() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = ControlPlane::new(
            format!("http://{}", listener.local_addr().unwrap()),
            Some("token".into()),
        );
        let accepted = json!({"operation_id":"op-stalled","state":"removing"});
        let (connected, confirmed) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            connected.send(()).unwrap();
            // Keep the connection open without responding until the test cancels it.
            std::future::pending::<()>().await;
            drop(stream);
        });
        let error = api
            .wait_for_hub_skill_operation(
                &accepted,
                HubSkillWaitOptions {
                    timeout: Duration::from_millis(200),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        confirmed.await.unwrap();
        assert!(matches!(error, ThalovantError::Timeout(_)));
        assert!(error.to_string().contains("op-stalled"));
        assert_eq!(accepted["state"], "removing");
        worker.abort();
    }
    #[tokio::test]
    async fn invalid_arguments_do_not_send() {
        let api = ControlPlane::new("https://invalid.example", Some("token".into()));
        assert!(api.list_hub_skill_history("h", 0).await.is_err());
        assert!(api.list_hub_skill_history("h", 201).await.is_err());
        assert!(api
            .install_hub_skill(
                "h",
                "s",
                "latest",
                HubSkillWaitOptions {
                    timeout: Duration::ZERO,
                    ..Default::default()
                }
            )
            .await
            .is_err());
    }
}
