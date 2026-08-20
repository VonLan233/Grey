use grey_core::{GreyConfig, ProviderAuth};

#[test]
fn provider_auth_defaults_to_api_key_and_parses_chatgpt_oauth() {
    let defaults = GreyConfig::default();
    assert_eq!(defaults.providers[0].auth, ProviderAuth::ApiKey);

    let config: GreyConfig = toml::from_str(
        r#"
        [[providers]]
        id = "chatgpt"
        protocol = "openai_responses"
        auth = "chatgpt_oauth"
        "#,
    )
    .unwrap();
    assert_eq!(config.providers[0].auth, ProviderAuth::ChatgptOauth);
}
