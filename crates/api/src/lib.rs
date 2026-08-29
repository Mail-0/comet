#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
}

impl Client {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    pub fn list_agents_request(&self) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}/api/webapp/agents", self.base_url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_joining_normalizes_the_base_url() {
        let client = Client::new("https://keiki.example/");

        let request = client.list_agents_request().build().unwrap();

        assert_eq!(
            request.url().as_str(),
            "https://keiki.example/api/webapp/agents"
        );
    }
}
