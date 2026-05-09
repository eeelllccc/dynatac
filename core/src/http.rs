//! HTTP client trait and mock implementation.
//!
//! The trait defines a minimal HTTP interface for programs that need
//! network requests. `MockHttpClient` provides a test double with
//! canned responses.
//!
//! Driver invariants:
//!   - `get(url)` returns the response body or an error string.
//!   - `get_with_bearer` and `post_json_with_bearer` behave the same but
//!     include an `Authorization: Bearer` header on the real implementation.

/// Hardware-agnostic HTTP client interface.
pub trait HttpClient {
    fn get(&mut self, url: &str) -> Result<String, String>;
    fn get_with_bearer(&mut self, url: &str, token: &str) -> Result<String, String>;
    fn post_json_with_bearer(&mut self, url: &str, body: &str, token: &str) -> Result<String, String>;
}

/// Test double: returns canned responses keyed by URL.
pub struct MockHttpClient {
    responses: Vec<(String, Result<String, String>)>,
    bearer_responses: Vec<(String, Result<String, String>)>,
    post_responses: Vec<(String, Result<String, String>)>,
}

impl MockHttpClient {
    pub fn new() -> Self {
        Self {
            responses: Vec::new(),
            bearer_responses: Vec::new(),
            post_responses: Vec::new(),
        }
    }

    pub fn on_get(&mut self, url: &str, response: Result<String, String>) {
        self.responses.push((url.to_string(), response));
    }

    pub fn on_get_with_bearer(&mut self, url: &str, response: Result<String, String>) {
        self.bearer_responses.push((url.to_string(), response));
    }

    pub fn on_post_json_with_bearer(&mut self, url: &str, response: Result<String, String>) {
        self.post_responses.push((url.to_string(), response));
    }
}

impl HttpClient for MockHttpClient {
    fn get(&mut self, url: &str) -> Result<String, String> {
        for (u, r) in &self.responses {
            if u == url {
                return r.clone();
            }
        }
        Err(format!("no mock response for: {}", url))
    }

    fn get_with_bearer(&mut self, url: &str, _token: &str) -> Result<String, String> {
        for (u, r) in &self.bearer_responses {
            if u == url {
                return r.clone();
            }
        }
        Err(format!("no mock bearer response for: {}", url))
    }

    fn post_json_with_bearer(&mut self, url: &str, _body: &str, _token: &str) -> Result<String, String> {
        for (u, r) in &self.post_responses {
            if u == url {
                return r.clone();
            }
        }
        Err(format!("no mock post response for: {}", url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_returns_canned_response() {
        let mut client = MockHttpClient::new();
        client.on_get("http://example.com", Ok("<html>hello</html>".into()));
        assert_eq!(client.get("http://example.com").unwrap(), "<html>hello</html>");
    }

    #[test]
    fn mock_returns_error_for_unknown_url() {
        let mut client = MockHttpClient::new();
        let err = client.get("http://unknown.com").unwrap_err();
        assert!(err.contains("no mock response"));
    }

    #[test]
    fn mock_returns_canned_error() {
        let mut client = MockHttpClient::new();
        client.on_get("http://fail.com", Err("connection refused".into()));
        assert_eq!(client.get("http://fail.com").unwrap_err(), "connection refused");
    }
}
