#![cfg(feature = "provider")]
use atomcode_capabilities::provider::{
    AnthropicConfig, AnthropicProvider, OllamaConfig, OllamaProvider, OpenAiCompatConfig,
    OpenAiCompatProvider, ResponsesProvider,
};
use atomcode_config::provider_headers::ProviderHeaders;
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn headers() -> ProviderHeaders {
    serde_json::from_str(r#"{"X-Custom-Auth":"private-token","Authorization":"Custom private-token","anthropic-version":"custom-version"}"#).unwrap()
}

fn provider(wire: &str, key: &str, url: &str) -> Box<dyn LlmProvider> {
    match wire {
        "anthropic" => {
            let mut c = AnthropicConfig::new(key, url.to_owned(), "test");
            c.headers = headers();
            Box::new(AnthropicProvider::new(c).unwrap())
        }
        "ollama" => {
            let mut c = OllamaConfig::new(url.to_owned(), "test");
            c.api_key = key.into();
            c.headers = headers();
            Box::new(OllamaProvider::new(c).unwrap())
        }
        _ => {
            let mut c = OpenAiCompatConfig::new(key, url.to_owned(), "test");
            c.headers = headers();
            if wire == "responses" {
                Box::new(ResponsesProvider::new(c).unwrap())
            } else {
                Box::new(OpenAiCompatProvider::new(c).unwrap())
            }
        }
    }
}

// Real HTTP is intentional: assert the bytes the server receives, including
// overriding built-in authentication. The 401 response never enters backoff.
#[tokio::test]
async fn every_chat_wire_sends_account_headers_and_redacts_echoed_credentials() {
    for wire in ["openai", "responses", "anthropic", "ollama"] {
        for key in ["", "ordinary-key"] {
            let server = MockServer::start().await;
            Mock::given(wiremock::matchers::method("POST"))
                .respond_with(ResponseTemplate::new(401).set_body_json(
                    serde_json::json!({"error":{"message":"rejected private-token"}}),
                ))
                .mount(&server)
                .await;
            let provider = provider(wire, key, &server.uri());
            let error = match provider
                .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
                .await
            {
                Err(e) => e,
                Ok(_) => panic!("401 must fail"),
            };
            assert!(
                !error.message.contains("private-token"),
                "{wire} leaked a credential"
            );
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            let r = &requests[0];
            assert_eq!(r.headers.get("x-custom-auth").unwrap(), "private-token");
            assert_eq!(
                r.headers.get("authorization").unwrap(),
                "Custom private-token"
            );
            assert_eq!(
                r.headers.get("anthropic-version").unwrap(),
                "custom-version"
            );
            if wire == "anthropic" && key.is_empty() {
                assert!(r.headers.get("x-api-key").is_none());
            }
        }
    }
}

#[tokio::test]
async fn discovery_and_probe_use_headers_without_an_api_key() {
    use atomcode_capabilities::provider::{
        discovery::{fetch_discovery_body, DiscoveryTransport},
        probe::{probe_chat_endpoint, ProbeTarget},
    };
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":[]})))
        .mount(&server)
        .await;
    Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(
                serde_json::json!({"error":{"message":"messages must not be empty"}}),
            ),
        )
        .mount(&server)
        .await;
    fetch_discovery_body(
        format!("{}/models", server.uri()).parse().unwrap(),
        "openai",
        &DiscoveryTransport {
            headers: headers(),
            ..Default::default()
        },
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();
    let verdict = probe_chat_endpoint(&ProbeTarget {
        base_url: server.uri(),
        headers: headers(),
        ..Default::default()
    })
    .await;
    assert!(verdict.is_ok(), "{verdict:?}");
    for r in server.received_requests().await.unwrap() {
        assert_eq!(r.headers.get("x-custom-auth").unwrap(), "private-token");
    }
}

#[tokio::test]
async fn custom_credentials_never_follow_a_redirect_to_another_server() {
    for wire in ["openai", "responses", "anthropic", "ollama"] {
        let source = MockServer::start().await;
        let destination = MockServer::start().await;
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", destination.uri()))
            .mount(&source)
            .await;
        let p = provider(wire, "", &source.uri());
        assert!(p
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .is_err());
        assert!(
            destination.received_requests().await.unwrap().is_empty(),
            "{wire} followed a credential-bearing redirect"
        );
    }
}
