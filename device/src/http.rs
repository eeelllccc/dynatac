//! Real HTTP client using ESP-IDF's HTTP client.
//!
//! Wraps `EspHttpConnection` and implements the `HttpClient` trait
//! from `dynatac_core`.
//!
//! Driver invariants:
//!   - WiFi must be connected before making requests
//!   - `get(url)` performs a blocking HTTP GET and returns the body as a String

use dynatac_core::http::HttpClient;

use esp_idf_svc::http::client::{Configuration, EspHttpConnection};

pub struct EspHttpClient;

impl EspHttpClient {
    pub fn new() -> Self {
        Self
    }
}

impl HttpClient for EspHttpClient {
    fn get(&mut self, url: &str) -> Result<String, String> {
        let config = Configuration {
            crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
            ..Default::default()
        };
        let mut conn =
            EspHttpConnection::new(&config).map_err(|e| format!("{:?}", e))?;

        conn.initiate_request(esp_idf_svc::http::Method::Get, url, &[])
            .map_err(|e| format!("{:?}", e))?;

        conn.initiate_response()
            .map_err(|e| format!("{:?}", e))?;

        let status = conn.status();
        if status < 200 || status >= 300 {
            return Err(format!("HTTP {}", status));
        }

        const MAX_BODY: usize = 16 * 1024;
        let mut body = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match conn.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let remaining = MAX_BODY - body.len();
                    if remaining == 0 {
                        break;
                    }
                    body.extend_from_slice(&buf[..n.min(remaining)]);
                }
                Err(e) => return Err(format!("read error: {:?}", e)),
            }
        }

        String::from_utf8(body).map_err(|e| format!("utf8 error: {}", e))
    }
}
