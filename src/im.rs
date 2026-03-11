use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde_json::{json, Value};

pub trait ImChannel {
    fn send_text(&self, text: &str) -> Result<()>;
}

pub struct FeishuChannel {
    pub webhook_url: String,
    pub client: Client,
}

pub struct TelegramChannel {
    pub bot_token: String,
    pub chat_id: String,
    pub message_thread_id: Option<i64>,
    pub client: Client,
}

impl ImChannel for FeishuChannel {
    fn send_text(&self, text: &str) -> Result<()> {
        let payload = json!({
            "msg_type": "text",
            "content": {
                "text": text,
            }
        });

        let resp = self
            .client
            .post(&self.webhook_url)
            .json(&payload)
            .send()
            .context("feishu request failed")?
            .error_for_status()
            .context("feishu http status error")?;

        let value: Value = resp.json().context("invalid feishu response json")?;

        if let Some(code) = value.get("code").and_then(|v| v.as_i64()) {
            if code != 0 {
                let msg = value
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown feishu error");
                bail!("feishu api error: code={} msg={}", code, msg);
            }
        }
        Ok(())
    }
}

impl ImChannel for TelegramChannel {
    fn send_text(&self, text: &str) -> Result<()> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);

        let mut payload = json!({
            "chat_id": self.chat_id,
            "text": text,
        });

        if let Some(thread_id) = self.message_thread_id {
            payload["message_thread_id"] = json!(thread_id);
        }

        let resp = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .context("telegram request failed")?
            .error_for_status()
            .context("telegram http status error")?;

        let value: Value = resp.json().context("invalid telegram response json")?;
        let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if !ok {
            let desc = value
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown telegram error");
            bail!("telegram api error: {}", desc);
        }
        Ok(())
    }
}
