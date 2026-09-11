//! Image generation.
//!
//! Two providers behind one call, because they disagree about more than the URL.
//! OpenRouter answers with the image inline as base64; fal answers with a link to
//! it, so the bytes take a second request that the caller should not have to know
//! about. Chat and embeddings stay on OpenRouter regardless of what is set here.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/images";
const FAL_URL: &str = "https://fal.run";

/// Where images are generated. Named in config rather than inferred from the
/// model id, since a provider prefix is a naming convention and not a promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    OpenRouter,
    Fal,
}

impl std::str::FromStr for Provider {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "openrouter" => Ok(Provider::OpenRouter),
            "fal" | "fal.ai" | "fal-ai" => Ok(Provider::Fal),
            other => bail!("unknown image provider '{other}'; expected 'openrouter' or 'fal'"),
        }
    }
}

pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

pub struct ImageGen {
    http: reqwest::Client,
    provider: Provider,
    model: String,
    openrouter_key: String,
    /// Absent unless fal is configured, so the missing key is reported as
    /// configuration rather than as an authentication failure from fal.
    fal_key: Option<String>,
}

impl ImageGen {
    pub fn new(
        provider: Provider,
        model: String,
        openrouter_key: String,
        fal_key: Option<String>,
        timeout_s: u64,
    ) -> Result<Self> {
        if provider == Provider::Fal && fal_key.is_none() {
            bail!("image provider is 'fal' but FAL_API_KEY is not set");
        }
        let http = reqwest::Client::builder()
            .read_timeout(Duration::from_secs(timeout_s))
            .connect_timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            provider,
            model,
            openrouter_key,
            fal_key,
        })
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Generate one image. `model` overrides the configured one for this call,
    /// but not the provider: an override is a model on whichever provider is set.
    pub async fn generate(&self, prompt: &str, model: Option<&str>) -> Result<GeneratedImage> {
        let model = model.unwrap_or(&self.model);
        match self.provider {
            Provider::OpenRouter => self.openrouter(prompt, model).await,
            Provider::Fal => self.fal(prompt, model).await,
        }
    }

    async fn openrouter(&self, prompt: &str, model: &str) -> Result<GeneratedImage> {
        let payload = self
            .post(
                OPENROUTER_URL,
                &json!({ "model": model, "prompt": prompt }),
                |req| req.bearer_auth(&self.openrouter_key),
                "OpenRouter images",
            )
            .await?;

        let first = payload
            .pointer("/data/0")
            .context("image response had no data")?;
        let b64 = first
            .get("b64_json")
            .and_then(Value::as_str)
            .context("image response had no b64_json")?;
        let media_type = first
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or("image/png")
            .to_string();

        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .context("decoding image base64")?;
        Ok(GeneratedImage { bytes, media_type })
    }

    async fn fal(&self, prompt: &str, model: &str) -> Result<GeneratedImage> {
        let key = self
            .fal_key
            .as_ref()
            .context("image provider is 'fal' but FAL_API_KEY is not set")?;
        let url = fal_endpoint(model);

        // num_images rather than the default, so a model that would happily
        // return four does not cost four generations for one message.
        let payload = self
            .post(
                &url,
                &json!({ "prompt": prompt, "num_images": 1 }),
                |req| req.header("Authorization", format!("Key {key}")),
                "fal",
            )
            .await?;

        let first = payload
            .pointer("/images/0")
            .context("fal returned no images")?;
        let link = first
            .get("url")
            .and_then(Value::as_str)
            .context("fal image had no url")?;
        let media_type = first
            .get("content_type")
            .and_then(Value::as_str)
            .unwrap_or("image/png")
            .to_string();

        // fal hands back a link rather than the image, so the bytes are a second
        // request. The link is public and pre-signed; sending the key to it would
        // be leaking it to whatever host fal chose.
        let bytes = self
            .http
            .get(link)
            .send()
            .await
            .context("fetching the image fal generated")?
            .error_for_status()
            .context("fetching the image fal generated")?
            .bytes()
            .await
            .context("reading the image fal generated")?
            .to_vec();

        Ok(GeneratedImage { bytes, media_type })
    }

    /// POST JSON and parse the reply, reporting the provider's own error text.
    /// Shared because the failure modes are identical even though the auth is not.
    async fn post(
        &self,
        url: &str,
        body: &Value,
        auth: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
        who: &str,
    ) -> Result<Value> {
        let resp = auth(self.http.post(url))
            .json(body)
            .send()
            .await
            .with_context(|| format!("calling {who}"))?;

        let status = resp.status();
        let raw = resp
            .text()
            .await
            .with_context(|| format!("reading the reply from {who}"))?;
        let payload: Value = serde_json::from_str(&raw).map_err(|e| {
            anyhow::anyhow!(
                "{who} returned non-JSON ({status}): {e}: {}",
                crate::truncate(&raw, 300)
            )
        })?;

        if !status.is_success() {
            bail!("{who} {status}: {}", error_text(&payload));
        }
        Ok(payload)
    }
}

fn fal_endpoint(model: &str) -> String {
    format!("{FAL_URL}/{}", model.trim_matches('/'))
}

/// Pull the human-readable part out of an error body.
/// The two providers nest it differently, and fal's validation errors arrive as a
/// list of field complaints rather than a sentence.
fn error_text(payload: &Value) -> String {
    if let Some(message) = payload
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| payload.get("detail").and_then(Value::as_str))
    {
        return message.to_string();
    }
    if let Some(items) = payload.get("detail").and_then(Value::as_array) {
        let joined: Vec<String> = items
            .iter()
            .map(|item| {
                item.get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("invalid request")
                    .to_string()
            })
            .collect();
        if !joined.is_empty() {
            return joined.join("; ");
        }
    }
    "unknown error".to_string()
}
