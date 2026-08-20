use grey_core::GreyConfig;

const CODING_PLAN_URL: &str = "https://ark.cn-beijing.volces.com/api/coding/v3";
const OPENCODE_URL: &str = "https://opencode.ai/zen/v1";
const OPENCODE_GO_URL: &str = "https://opencode.ai/zen/go/v1";

#[test]
fn coding_plan_template_uses_the_dedicated_openai_compatible_endpoint() {
    let config: GreyConfig =
        toml::from_str(include_str!("../../../examples/volcano-coding-plan.toml"))
            .expect("Coding Plan template must be valid Grey TOML");
    let provider = config
        .providers
        .iter()
        .find(|provider| provider.id == "volcano-coding-plan")
        .expect("Coding Plan provider must be present");

    assert_eq!(provider.protocol, "openai");
    assert_eq!(provider.base_url, CODING_PLAN_URL);
    assert_ne!(
        provider.base_url,
        "https://ark.cn-beijing.volces.com/api/v3"
    );
    assert_eq!(provider.api_key, "${ARK_API_KEY}");
}

#[test]
fn opencode_template_keeps_each_documented_protocol_explicit() {
    let config: GreyConfig = toml::from_str(include_str!("../../../examples/opencode-go-zen.toml"))
        .expect("OpenCode template must be valid Grey TOML");
    let provider = |id: &str| {
        config
            .providers
            .iter()
            .find(|provider| provider.id == id)
            .unwrap_or_else(|| panic!("missing provider `{id}`"))
    };

    for (id, protocol, base_url) in [
        ("opencode-go", "openai", OPENCODE_GO_URL),
        ("opencode-zen-responses", "openai_responses", OPENCODE_URL),
        ("opencode-zen-chat", "openai", OPENCODE_URL),
        ("opencode-zen-anthropic", "anthropic", OPENCODE_URL),
        ("opencode-zen-gemini", "gemini", OPENCODE_URL),
    ] {
        let entry = provider(id);
        assert_eq!(entry.protocol, protocol, "wrong protocol for `{id}`");
        assert_eq!(entry.base_url, base_url, "wrong base URL for `{id}`");
        assert_eq!(entry.api_key, "${OPENCODE_API_KEY}");
    }
}
