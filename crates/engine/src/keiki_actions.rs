use super::*;
use base64::Engine as _;
use serde::Serialize;

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        "KeikiConversationAction"
            | "KeikiConversationInfo"
            | "EndKeikiGroup"
            | "CancelKeikiTask"
            | "ListKeikiTemplates"
            | "CreateKeikiAgent"
            | "KeikiPresetConnection"
            | "GetKeikiAvatar"
    )
}

impl Inner {
    pub(super) fn decorate_page(
        &self,
        scope: &Owner,
        agent: &str,
        page: &mut Value,
    ) -> Result<(), RpcError> {
        self.check(scope)?;
        if let Some(rows) = page["conversations"].as_array_mut() {
            for row in rows {
                if let Some(binding) = self
                    .store
                    .lookup(scope, agent, row["phone"].as_str().unwrap_or(""))
                    .map_err(failed)?
                {
                    if let Some(chat) = self.workspace.chat(&binding.chat_id).map_err(failed)? {
                        row["localTitle"] = json!(chat.title);
                        row["localArchived"] = json!(chat.archived);
                        row["localPinned"] =
                            json!(self.store.pinned(scope, &binding.chat_id).map_err(failed)?);
                    }
                }
            }
        }
        Ok(())
    }
    pub(super) fn api_result<T: Serialize>(
        &self,
        scope: &Owner,
        token: &str,
        result: Result<T, keiki_api::Error>,
    ) -> Result<Value, RpcError> {
        self.check(scope)?;
        match result {
            Ok(value) => serde_json::to_value(value).map_err(failed),
            Err(keiki_api::Error::Api { status, .. }) => {
                if status == reqwest::StatusCode::UNAUTHORIZED {
                    self.auth.reject_access_token(token);
                }
                Err(failed(format!("Keiki returned {status}")))
            }
            Err(_) => Err(failed(
                "Keiki request failed. Check your connection and retry.",
            )),
        }
    }

    fn conversation_info(&self, scope: &Owner, binding: &Binding) -> Result<Value, RpcError> {
        let chat = self.workspace.chat(&binding.chat_id).map_err(failed)?;
        Ok(
            json!({"conversationId":binding.conversation_id,"agentId":binding.agent_id,
            "title":chat.as_ref().and_then(|c| c.title.clone()),"archived":chat.as_ref().is_some_and(|c| c.archived),
            "pinned":self.store.pinned(scope, &binding.chat_id).map_err(failed)?,
            "peer":binding.conversation_id.starts_with("agent:"),"desktop":binding.conversation_id.starts_with("api:")}),
        )
    }

    pub(super) async fn action(
        &self,
        scope: &Owner,
        method: &str,
        params: &Value,
    ) -> Result<Value, RpcError> {
        let client = keiki_api::Client::new(self.auth.api_url());
        if matches!(method, "KeikiConversationAction" | "KeikiConversationInfo") {
            let binding = self.binding(scope, text(params, "chatId")?)?;
            if method == "KeikiConversationInfo" {
                return self.conversation_info(scope, &binding);
            }
            let action = text(params, "action")?;
            match action {
                "rename" => {
                    let title = text(params, "title")?.trim();
                    if title.is_empty() || title.chars().count() > 200 {
                        return Err(RpcError::BadParams(
                            "Enter a title of 1–200 characters".into(),
                        ));
                    }
                    self.workspace
                        .rename_chat(&binding.chat_id, title)
                        .map_err(failed)?;
                }
                "archive" | "restore" => {
                    self.workspace
                        .set_chat_archived(&binding.chat_id, action == "archive")
                        .map_err(failed)?;
                }
                "pin" | "unpin" => self
                    .store
                    .pin(scope, &binding.chat_id, action == "pin")
                    .map_err(failed)?,
                "deleteLocal" => {
                    self.sessions
                        .stop_turn(&binding.chat_id)
                        .await
                        .map_err(failed)?;
                    self.close_chat_terminals(scope, &binding.chat_id)?;
                    self.workspace
                        .delete_chat(&binding.chat_id)
                        .map_err(failed)?;
                    self.store.forget(scope, &binding.chat_id).map_err(failed)?;
                    return Ok(json!({"deletedLocal":true}));
                }
                "openDashboard" => {
                    let url = dashboard_url(self.auth.api_url(), &binding)?;
                    self.check(scope)?;
                    self.launch(&url)?;
                    return Ok(json!({"opened":true}));
                }
                "block" | "unblock" | "end" => {
                    let peer = binding.conversation_id.starts_with("agent:");
                    if (action == "end") != peer {
                        return Err(RpcError::BadParams(
                            "Use End for peer threads and Block for contact conversations".into(),
                        ));
                    }
                    let token = self.authenticated(scope).await?;
                    if peer {
                        return self.api_result(
                            scope,
                            &token,
                            client
                                .end_agent_thread(&token, &locator(&binding), false)
                                .await
                                .map(|r| json!({"ended":r.ended,"settled":r.settled})),
                        );
                    }
                    return self.api_result(
                        scope,
                        &token,
                        client
                            .set_conversation_blocked(&token, &locator(&binding), action == "block")
                            .await
                            .map(|r| json!({"blocked":r.blocked})),
                    );
                }
                _ => {
                    return Err(RpcError::BadParams(
                        "Unsupported conversation action".into(),
                    ));
                }
            }
            return self.conversation_info(scope, &binding);
        }
        let token = self.authenticated(scope).await?;
        match method {
            "EndKeikiGroup" => {
                if params["confirmed"] != true {
                    return Err(RpcError::BadParams("Confirm ending group work".into()));
                }
                self.api_result(
                    scope,
                    &token,
                    client
                        .end_agent_group(&token, text(params, "groupId")?)
                        .await,
                )
            }
            "CancelKeikiTask" => {
                let binding = self.binding(scope, text(params, "chatId")?)?;
                let task = text(params, "taskId")?;
                let detail = self
                    .page(scope, &binding.agent_id, &binding.conversation_id, 1, None)
                    .await?;
                if !detail["tasks"]
                    .as_array()
                    .is_some_and(|tasks| tasks.iter().any(|row| row["id"] == task))
                {
                    return Err(RpcError::BadParams(
                        "Task is not part of this conversation".into(),
                    ));
                }
                self.api_result(
                    scope,
                    &token,
                    client.cancel_subagent_task(&token, task).await,
                )
            }
            "ListKeikiTemplates" => {
                self.request(scope, |token| {
                    Ok(client.list_agent_templates_authenticated_request(token))
                })
                .await
            }
            "CreateKeikiAgent" => {
                let input: keiki_api::CreateAgentFromTemplate =
                    serde_json::from_value(params.clone())
                        .map_err(|_| RpcError::BadParams("Invalid agent template input".into()))?;
                if input.template.trim().is_empty()
                    || input
                        .name
                        .as_ref()
                        .is_some_and(|name| name.trim().is_empty() || name.chars().count() > 100)
                {
                    return Err(RpcError::BadParams(
                        "Choose a template and a valid name".into(),
                    ));
                }
                self.api_result(
                    scope,
                    &token,
                    client.create_agent_from_template(&token, &input).await,
                )
            }
            "KeikiPresetConnection" => {
                let binding = self.binding(scope, text(params, "chatId")?)?;
                let preset_id = text(params, "presetId")?;
                let questions = self
                    .store
                    .questions(scope, &binding.chat_id)
                    .map_err(failed)?;
                let spec = mcp_spec(&questions, preset_id).ok_or_else(|| {
                    RpcError::BadParams("Connection is not pending in this conversation".into())
                })?;
                if spec["agentId"] != binding.agent_id
                    || preset_id.len() > 256
                    || !preset_id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err(RpcError::BadParams("Invalid connection".into()));
                }
                let result = if params["refresh"] == true {
                    client
                        .refresh_mcp_preset_status(&token, &binding.agent_id, preset_id)
                        .await
                } else {
                    client
                        .connect_mcp_preset(&token, &binding.agent_id, preset_id)
                        .await
                };
                self.check(scope)?;
                match result {
                    Ok(mut preset) => {
                        let url = preset.authorization_url.take();
                        let opened = if preset.status == "needs_auth" && params["refresh"] != true {
                            if let Some(url) = url {
                                self.launch(&url)?;
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        Ok(
                            json!({"id":preset.id,"name":preset.name,"status":preset.status,"opened":opened}),
                        )
                    }
                    Err(error) => self.api_result::<Value>(scope, &token, Err(error)),
                }
            }
            "GetKeikiAvatar" => {
                let agent = text(params, "agentId")?;
                let state: keiki_api::AvatarState =
                    serde_json::from_value(params.get("state").cloned().unwrap_or(json!("idle")))
                        .map_err(|_| RpcError::BadParams("Invalid avatar state".into()))?;
                let theme: keiki_api::AvatarTheme =
                    serde_json::from_value(params.get("theme").cloned().unwrap_or(json!("dark")))
                        .map_err(|_| RpcError::BadParams("Invalid avatar theme".into()))?;
                let key = json!([scope.account, scope.org, agent, state, theme]).to_string();
                if let Some((_, value)) = self
                    .avatars
                    .lock()
                    .map_err(|_| failed("Avatar cache is unavailable"))?
                    .iter()
                    .find(|(entry, _)| entry == &key)
                {
                    return Ok(json!({"src":value}));
                }
                let agents = self
                    .request(scope, |token| {
                        Ok(client.list_agents_authenticated_request(token))
                    })
                    .await?;
                if !agents["agents"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|row| row["id"] == agent))
                {
                    return Err(failed("Agent is not in this organization"));
                }
                let bytes = client
                    .agent_avatar(agent, 64, state, theme)
                    .await
                    .map_err(|_| failed("Avatar could not be loaded"))?;
                self.check(scope)?;
                if bytes.len() > 1_048_576 {
                    return Err(failed("Avatar is too large"));
                }
                let src = avatar_uri(&bytes)?;
                let mut cache = self
                    .avatars
                    .lock()
                    .map_err(|_| failed("Avatar cache is unavailable"))?;
                if cache.len() >= 64 {
                    cache.remove(0);
                }
                cache.push((key, src.clone()));
                Ok(json!({"src":src}))
            }
            _ => Err(RpcError::UnknownMethod(method.into())),
        }
    }

    fn launch(&self, url: &str) -> Result<(), RpcError> {
        let parsed = reqwest::Url::parse(url).map_err(|_| failed("Invalid browser URL"))?;
        if !matches!(parsed.scheme(), "https" | "http")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(failed("Unsupported browser URL"));
        }
        self.opener
            .lock()
            .map_err(|_| failed("Browser is unavailable"))?
            .as_ref()(url)
        .map_err(failed)
    }
}

fn avatar_uri(bytes: &[u8]) -> Result<String, RpcError> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| failed("Invalid avatar image"))?;
    let format = reader
        .format()
        .ok_or_else(|| failed("Invalid avatar image"))?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| failed("Invalid avatar image"))?;
    if width == 0 || height == 0 || width > 256 || height > 256 {
        return Err(failed("Invalid avatar dimensions"));
    }
    Ok(format!(
        "data:{};base64,{}",
        format.to_mime_type(),
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase4_avatar_preserves_gif_and_rejects_empty_or_invalid_images() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode("R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7")
            .unwrap();
        let uri = avatar_uri(&bytes).unwrap();
        assert!(uri.starts_with("data:image/gif;base64,"));
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(uri.split_once(',').unwrap().1)
                .unwrap(),
            bytes
        );
        assert!(avatar_uri(&[]).is_err());
        assert!(avatar_uri(b"not an image").is_err());
    }
}

fn mcp_spec<'a>(value: &'a Value, preset: &str) -> Option<&'a Value> {
    match value {
        Value::Object(fields) => {
            if let Some(spec) = fields
                .get("mcpConnect")
                .filter(|spec| spec["presetId"] == preset)
            {
                return Some(spec);
            }
            fields.values().find_map(|value| mcp_spec(value, preset))
        }
        Value::Array(values) => values.iter().find_map(|value| mcp_spec(value, preset)),
        _ => None,
    }
}

fn dashboard_url(base: &str, binding: &Binding) -> Result<String, RpcError> {
    let mut url = reqwest::Url::parse(base).map_err(|_| failed("Invalid Keiki URL"))?;
    url.set_query(None);
    url.set_fragment(None);
    url.path_segments_mut()
        .map_err(|_| failed("Invalid Keiki URL"))?
        .pop_if_empty()
        .push("conversations")
        .push(&binding.conversation_id);
    url.query_pairs_mut()
        .append_pair("agentId", &binding.agent_id);
    Ok(url.into())
}

pub(super) fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = tokio::process::Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = tokio::process::Command::new("open");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = tokio::process::Command::new("xdg-open");
    let mut child = command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| "Could not open your browser".to_owned())?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}
