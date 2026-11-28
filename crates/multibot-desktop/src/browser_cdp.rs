use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use multibot_core::browser_pane::parse_devtools_active_port;
use multibot_proto::{BrowserCommand, BrowserResponse, BrowserTarget, MAX_BROWSER_RESULT_BYTES};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{net::TcpStream, sync::Mutex};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

#[derive(Clone)]
pub(crate) struct BrowserController {
    profile: PathBuf,
    active_target: Arc<Mutex<Option<String>>>,
}

impl BrowserController {
    pub(crate) fn new(profile: PathBuf) -> Self {
        Self {
            profile,
            active_target: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) async fn execute(&self, command: BrowserCommand) -> BrowserResponse {
        let result = tokio::time::timeout(Duration::from_secs(15), self.run(command)).await;
        match result {
            Ok(Ok(response)) => match serde_json::to_vec(&response) {
                Ok(bytes) if bytes.len() <= MAX_BROWSER_RESULT_BYTES => response,
                _ => BrowserResponse::Error {
                    message: "Browser result exceeds 512 KiB; narrow the query or screenshot clip"
                        .into(),
                },
            },
            Ok(Err(error)) => BrowserResponse::Error {
                message: error.to_string(),
            },
            Err(_) => BrowserResponse::Error {
                message: "Browser command timed out".into(),
            },
        }
    }

    async fn run(&self, command: BrowserCommand) -> Result<BrowserResponse> {
        // Serializing requests also makes tab activation deterministic for this pane only.
        let mut active = self.active_target.lock().await;
        let contents = tokio::fs::read_to_string(self.profile.join("DevToolsActivePort"))
            .await
            .context("Browser CDP endpoint is not ready")?;
        let (port, path) =
            parse_devtools_active_port(&contents).context("Invalid DevToolsActivePort")?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(2 * 1024 * 1024))
            .max_frame_size(Some(2 * 1024 * 1024));
        // The browser UUID pins this connection to the launched process, not just a reusable port.
        let (socket, _) =
            connect_async_with_config(format!("ws://127.0.0.1:{port}{path}"), Some(config), true)
                .await?;
        let mut cdp = Cdp { socket, next_id: 0 };
        let targets = cdp.call("Target.getTargets", json!({}), None).await?;
        let pages = targets["targetInfos"]
            .as_array()
            .context("Missing CDP targets")?;
        let tabs: Vec<BrowserTarget> = pages
            .iter()
            .filter(|p| p["type"] == "page")
            .filter_map(|p| {
                Some(BrowserTarget {
                    target_id: p["targetId"].as_str()?.into(),
                    url: p["url"].as_str().unwrap_or_default().into(),
                    title: p["title"].as_str().unwrap_or_default().into(),
                })
            })
            .collect();
        if matches!(command, BrowserCommand::Tabs) {
            return Ok(BrowserResponse::Tabs { tabs });
        }
        if let BrowserCommand::ActivateTab { target_id } = &command {
            ensure!(
                tabs.iter().any(|tab| tab.target_id == *target_id),
                "Target does not belong to this session's browser"
            );
            cdp.call(
                "Target.activateTarget",
                json!({"targetId": target_id}),
                None,
            )
            .await?;
            *active = Some(target_id.clone());
            return Ok(BrowserResponse::Value {
                value: json!({"target_id": target_id}),
            });
        }
        let target = active
            .as_ref()
            .and_then(|id| tabs.iter().find(|t| t.target_id == *id))
            .or_else(|| tabs.first())
            .context("No page in this session's browser")?;
        *active = Some(target.target_id.clone());
        let attached = cdp
            .call(
                "Target.attachToTarget",
                json!({"targetId": target.target_id, "flatten": true}),
                None,
            )
            .await?;
        let session = attached["sessionId"]
            .as_str()
            .context("Missing CDP session")?;
        let value = match command {
            BrowserCommand::Navigate { url } => {
                validate_url(&url)?;
                let result = cdp
                    .call("Page.navigate", json!({"url": url}), Some(session))
                    .await?;
                if let Some(error) = result["errorText"].as_str() {
                    bail!("Navigation failed: {error}");
                }
                result
            }
            BrowserCommand::Eval { expression } => cdp.evaluate(&expression, session).await?,
            BrowserCommand::ReadText => {
                cdp.evaluate("document.body?.innerText ?? ''", session)
                    .await?
            }
            BrowserCommand::Click { selector } => {
                let selector = serde_json::to_string(&selector)?;
                cdp.evaluate(&format!("(() => {{ const e = document.querySelector({selector}); if (!e) throw new Error('Element not found'); e.click(); return true; }})()"), session).await?
            }
            BrowserCommand::Type { selector, text } => {
                let selector = serde_json::to_string(&selector)?;
                let text = serde_json::to_string(&text)?;
                cdp.evaluate(&format!("(() => {{ const e = document.querySelector({selector}); if (!e) throw new Error('Element not found'); e.focus(); if (e.isContentEditable) e.textContent = {text}; else {{ const p = e instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype; const set = Object.getOwnPropertyDescriptor(p, 'value').set; set.call(e, {text}); }} e.dispatchEvent(new InputEvent('input', {{bubbles:true, inputType:'insertText', data:{text}}})); e.dispatchEvent(new Event('change', {{bubbles:true}})); return true; }})()"), session).await?
            }
            BrowserCommand::Screenshot => {
                cdp.call(
                    "Page.captureScreenshot",
                    json!({"format":"png", "captureBeyondViewport":false}),
                    Some(session),
                )
                .await?
            }
            BrowserCommand::RawCdp { method, params } => {
                validate_raw(&method, &params)?;
                cdp.call(&method, params, Some(session)).await?
            }
            BrowserCommand::Tabs | BrowserCommand::ActivateTab { .. } => unreachable!(),
        };
        Ok(BrowserResponse::Value { value })
    }
}

fn validate_url(url: &str) -> Result<()> {
    ensure!(
        url == "about:blank"
            || url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https")),
        "Only HTTP, HTTPS and about:blank navigation is allowed"
    );
    Ok(())
}

fn validate_raw(method: &str, params: &Value) -> Result<()> {
    // Browser/Target/IO commands can escape the attached page. Never forward them.
    ensure!(
        matches!(
            method,
            "Page.navigate"
                | "Page.reload"
                | "Page.captureScreenshot"
                | "Page.getLayoutMetrics"
                | "Runtime.evaluate"
                | "Runtime.callFunctionOn"
                | "Runtime.getProperties"
                | "DOM.getDocument"
                | "DOM.querySelector"
                | "DOM.querySelectorAll"
                | "DOM.getOuterHTML"
                | "DOM.getBoxModel"
                | "DOM.focus"
                | "Input.dispatchMouseEvent"
                | "Input.dispatchKeyEvent"
                | "Input.insertText"
        ),
        "Raw CDP method is not page-scoped or supported"
    );
    ensure!(params.is_object(), "CDP params must be an object");
    ensure!(
        params.get("sessionId").is_none() && params.get("targetId").is_none(),
        "Explicit CDP routing is forbidden"
    );
    if method == "Page.navigate" {
        validate_url(params["url"].as_str().context("Missing navigation URL")?)?;
    }
    Ok(())
}

struct Cdp {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl Cdp {
    async fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut request = json!({"id":id, "method":method, "params":params});
        if let Some(session) = session {
            request["sessionId"] = session.into();
        }
        self.socket
            .send(Message::Text(request.to_string().into()))
            .await?;
        while let Some(message) = self.socket.next().await {
            match message? {
                Message::Text(text) => {
                    let response: Value = serde_json::from_str(&text)?;
                    if response["id"].as_u64() != Some(id) {
                        continue;
                    }
                    if let Some(error) = response.get("error") {
                        bail!("CDP error: {error}");
                    }
                    return response
                        .get("result")
                        .cloned()
                        .context("Missing CDP result");
                }
                Message::Ping(data) => self.socket.send(Message::Pong(data)).await?,
                Message::Close(_) => break,
                _ => {}
            }
        }
        bail!("Browser CDP connection closed")
    }

    async fn evaluate(&mut self, expression: &str, session: &str) -> Result<Value> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({"expression":expression, "returnByValue":true, "awaitPromise":true}),
                Some(session),
            )
            .await?;
        if let Some(error) = result.get("exceptionDetails") {
            bail!("JavaScript exception: {error}");
        }
        Ok(result["result"]["value"].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn loopback_cdp_commands_and_target_isolation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = std::env::temp_dir().join(format!("multibot-cdp-{}-{port}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("DevToolsActivePort"),
            format!("{port}\n/devtools/browser/test-a\n"),
        )
        .unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(Message::Text(text))) = ws.next().await {
                        let req: Value = serde_json::from_str(&text).unwrap();
                        let method = req["method"].as_str().unwrap();
                        let result = match method {
                            "Target.getTargets" => {
                                json!({"targetInfos":[{"targetId":"page-a","type":"page","url":"about:blank","title":"A"}]})
                            }
                            "Target.attachToTarget" => {
                                assert_eq!(req["params"]["targetId"], "page-a");
                                json!({"sessionId":"attached-a"})
                            }
                            "Page.navigate" => {
                                assert_eq!(req["sessionId"], "attached-a");
                                assert_eq!(req["params"]["url"], "https://example.com");
                                json!({"frameId":"frame-a"})
                            }
                            "Runtime.evaluate" => {
                                assert_eq!(req["sessionId"], "attached-a");
                                let value = if req["params"]["expression"] == "1 + 1" {
                                    json!(2)
                                } else {
                                    json!("Hello from page A")
                                };
                                json!({"result":{"value":value}})
                            }
                            "Page.captureScreenshot" => {
                                assert_eq!(req["sessionId"], "attached-a");
                                json!({"data":"iVBORw0KGgo="})
                            }
                            _ => panic!("Unexpected method {method}"),
                        };
                        // Unsolicited events must not be confused with responses.
                        ws.send(Message::Text(
                            json!({"method":"Page.loadEventFired","params":{}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                        ws.send(Message::Text(
                            json!({"id":req["id"],"result":result}).to_string().into(),
                        ))
                        .await
                        .unwrap();
                    }
                });
            }
        });
        let controller = BrowserController::new(dir.clone());
        assert_eq!(
            controller
                .execute(BrowserCommand::Navigate {
                    url: "https://example.com".into()
                })
                .await,
            BrowserResponse::Value {
                value: json!({"frameId":"frame-a"})
            }
        );
        assert_eq!(
            controller
                .execute(BrowserCommand::Eval {
                    expression: "1 + 1".into()
                })
                .await,
            BrowserResponse::Value { value: json!(2) }
        );
        assert_eq!(
            controller.execute(BrowserCommand::ReadText).await,
            BrowserResponse::Value {
                value: json!("Hello from page A")
            }
        );
        assert_eq!(
            controller.execute(BrowserCommand::Screenshot).await,
            BrowserResponse::Value {
                value: json!({"data":"iVBORw0KGgo="})
            }
        );
        assert!(matches!(
            controller
                .execute(BrowserCommand::ActivateTab {
                    target_id: "page-b".into()
                })
                .await,
            BrowserResponse::Error { .. }
        ));
        assert!(matches!(
            controller
                .execute(BrowserCommand::RawCdp {
                    method: "Target.activateTarget".into(),
                    params: json!({"targetId":"page-b"})
                })
                .await,
            BrowserResponse::Error { .. }
        ));
        server.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn raw_commands_cannot_override_routing_or_use_browser_methods() {
        for method in [
            "Target.getTargets",
            "Browser.close",
            "Target.attachToTarget",
            "Target.sendMessageToTarget",
            "IO.read",
        ] {
            assert!(validate_raw(method, &json!({})).is_err());
        }
        assert!(validate_raw("Runtime.evaluate", &json!({"sessionId":"other"})).is_err());
        assert!(validate_raw("Page.navigate", &json!({"url":"file:///private"})).is_err());
        assert!(validate_raw("Runtime.evaluate", &json!({"expression":"1"})).is_ok());
    }
}
