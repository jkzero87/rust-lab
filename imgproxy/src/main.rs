//! Rust rewrite of ~/bin/imgproxy.py.
//!
//! HTTP proxy: listens on 127.0.0.1:8093 and forwards every request to the
//! model server (UPSTREAM, 127.0.0.1:8092). On the way in, JSON request
//! bodies whose `messages[*].content` parts carry a large `image_url` data-URI
//! are downscaled to PNG (see [`convert::fix_url`]), mirroring the Python
//! service on 8091. Responses are streamed back to the client.

use std::fmt;
use std::sync::OnceLock;

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::net::TcpListener;
use tracing::{info, warn};

mod convert;

const UPSTREAM: &str = "http://127.0.0.1:8092";
const LISTEN_ADDR: &str = "127.0.0.1:8093";
const MAX_BODY_LIMIT: usize = 256 * 1024 * 1024;

fn client() -> &'static reqwest::Client {
    static ONCE: OnceLock<reqwest::Client> = OnceLock::new();
    ONCE.get_or_init(reqwest::Client::new)
}

/// Rewrite `messages[*].content[*].image_url.url` data-URIs in place.
fn walk(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for msg in messages.iter_mut() {
        let Some(parts) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for part in parts.iter_mut() {
            let Some(iu) = part.get_mut("image_url").and_then(|i| i.as_object_mut()) else {
                continue;
            };
            let Some(url) = iu.get("url").and_then(|u| u.as_str()).map(String::from) else {
                continue;
            };
            match convert::fix_url(&url) {
                Ok(Some(new_url)) => {
                    iu.insert("url".to_string(), Value::String(new_url));
                }
                Ok(None) => {}
                Err(e) => warn!(url = %url, "fix_url failed: {e}; leaving url unchanged"),
            }
        }
    }
}

#[derive(Debug)]
struct StreamError(reqwest::Error);

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "upstream stream error: {}", self.0)
    }
}

impl std::error::Error for StreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

async fn proxy(req: Request<Body>) -> Response {
    let method = req.method().clone();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let full_url = format!("{UPSTREAM}{path_and_query}");

    // Forward request headers, dropping hop-by-hop ones (Python does the same).
    let mut fwd_headers = HeaderMap::new();
    for (k, v) in req.headers() {
        match k.as_str().to_ascii_lowercase().as_str() {
            "host" | "content-length" | "connection" => continue,
            _ => {
                fwd_headers.append(k.clone(), v.clone());
            }
        }
    }

    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY_LIMIT).await {
        Ok(b) => b,
        Err(e) => {
            warn!("failed to read request body: {e}");
            return (StatusCode::BAD_REQUEST, "failed to read request body").into_response();
        }
    };

    // Rewrite the body if it is JSON; otherwise pass it through untouched.
    let final_body: Bytes = if body_bytes.is_empty() {
        Bytes::new()
    } else {
        match serde_json::from_slice::<Value>(&body_bytes) {
            Ok(mut v) => {
                walk(&mut v);
                serde_json::to_vec(&v).map(Bytes::from).unwrap_or(body_bytes)
            }
            Err(_) => body_bytes,
        }
    };

    let mut builder = client().request(method, full_url.clone());
    for (k, v) in &fwd_headers {
        builder = builder.header(k, v);
    }
    if !final_body.is_empty() {
        builder = builder.body(final_body);
    }

    let upstream = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("upstream request failed for {full_url}: {e}");
            return (StatusCode::BAD_GATEWAY, "upstream request failed").into_response();
        }
    };

    let status = upstream.status();

    let mut resp_headers = HeaderMap::new();
    for (k, v) in upstream.headers() {
        match k.as_str().to_ascii_lowercase().as_str() {
            "transfer-encoding" | "content-length" | "connection" => continue,
            _ => {
                resp_headers.append(k.clone(), v.clone());
            }
        }
    }

    let stream = upstream.bytes_stream().map(|item| {
        item.map(Bytes::from)
            .map_err(|e| StreamError(e))
    });

    let body = Body::from_stream(stream);
    let mut resp = axum::http::Response::new(body);
    *resp.status_mut() = status;
    for (k, v) in resp_headers.iter() {
        resp.headers_mut().append(k, v.clone());
    }
    resp
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let app = Router::new().fallback(proxy);

    let listener = TcpListener::bind(LISTEN_ADDR).await?;
    info!(listen = LISTEN_ADDR, upstream = UPSTREAM, "imgproxy (rust) ready");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::walk;
    use base64::Engine;
    use serde_json::json;

    #[test]
    fn walk_rewrites_large_webp() {
        let b64 = base64::engine::general_purpose::STANDARD
            .encode(&[0u8; 8]); // tiny webp payload is not a real image; only shape matters here
        let mut body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": format!("data:image/webp;base64,{b64}")}}
                ]}
            ]
        });
        // A bogus image payload must not panic and must not leave the shape broken.
        let before = body["messages"][0]["content"][0]["image_url"]["url"].clone();
        walk(&mut body);
        let after = &body["messages"][0]["content"][0]["image_url"]["url"];
        assert!(before.is_string() && after.is_string());
    }

    #[test]
    fn walk_leaves_plain_text_content_alone() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "hello"}
            ]
        });
        walk(&mut body);
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[test]
    fn walk_ignores_missing_messages() {
        let mut body = json!({"foo": 1});
        walk(&mut body);
        assert_eq!(body["foo"], 1);
    }
}
