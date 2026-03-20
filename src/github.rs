use anyhow::{bail, Context, Result};
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// HTTP client for GitHub REST and GraphQL APIs.
pub struct GitHubClient {
    /// Underlying HTTP client.
    client: reqwest::Client,
    /// Personal access token.
    token: String,
}

impl GitHubClient {
    /// Create a new client with the given token.
    pub fn new(token: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            token,
        }
    }

    /// Exponential backoff wait time for rate limit retries.
    fn rate_limit_wait(attempt: u32) -> u64 {
        60 * (1 << attempt).min(8)
    }

    /// GET a URL and deserialize the JSON response, with rate limit retry.
    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        for attempt in 0..5 {
            let resp = self
                .client
                .get(url)
                .header(AUTHORIZATION, format!("Bearer {}", self.token))
                .header(USER_AGENT, "tracker")
                .header(ACCEPT, "application/vnd.github.v3+json")
                .send()
                .await?;

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    let wait = Self::rate_limit_wait(attempt);
                    log::warn!("Rate limited on {url} (HTTP {status}), retrying in {wait}s (attempt {}/5)", attempt + 1);
                    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                    continue;
                }
                bail!("GET {url} returned {status}: {body}");
            }
            return Ok(resp.json().await?);
        }
        bail!("GET {url} rate limit exceeded after 5 retries")
    }

    /// Fetch raw file content (for large files that exceed the contents API limit).
    pub async fn get_raw_content(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<String> {
        let url = format!(
            "https://raw.githubusercontent.com/{owner}/{repo}/{git_ref}/{path}"
        );
        let resp = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header(USER_AGENT, "tracker")
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("GET {url} returned {status}: {body}");
        }
        Ok(resp.text().await?)
    }

    /// Execute a GraphQL query with rate limit retry.
    pub async fn graphql_query(&self, query: &str, variables: Value) -> Result<Value> {
        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });

        for attempt in 0..5 {
            let resp = self
                .client
                .post("https://api.github.com/graphql")
                .header(AUTHORIZATION, format!("Bearer {}", self.token))
                .header(USER_AGENT, "tracker")
                .json(&body)
                .send()
                .await?;

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    let wait = Self::rate_limit_wait(attempt);
                    log::warn!("Rate limited (HTTP {status}), retrying in {wait}s (attempt {}/5)", attempt + 1);
                    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                    continue;
                }
                bail!("GraphQL request returned {status}: {body}");
            }

            let result: Value = resp.json().await?;
            if let Some(errors) = result.get("errors") {
                let err_str = errors.to_string();
                if err_str.contains("RATE_LIMIT") {
                    let wait = Self::rate_limit_wait(attempt);
                    log::warn!("GraphQL rate limited, retrying in {wait}s (attempt {}/5)", attempt + 1);
                    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                    continue;
                }
                bail!("GraphQL errors: {errors}");
            }
            return Ok(result);
        }

        bail!("GraphQL rate limit exceeded after 5 retries")
    }

    /// Get latest commit SHA on a branch.
    pub async fn get_latest_commit(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<String> {
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/commits/{branch}"
        );
        let resp: Value = self.get_json(&url).await?;
        resp["sha"]
            .as_str()
            .map(String::from)
            .context("no sha in commit response")
    }
}

