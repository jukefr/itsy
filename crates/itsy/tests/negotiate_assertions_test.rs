/// Integration test for dual-model assertion negotiation.
/// Calls the live llama-server at localhost:8000.
/// Run with:  cargo test --test negotiate_assertions_test -- --nocapture

#[tokio::test(flavor = "multi_thread")]
async fn negotiate_assertions_roundtrip() {
    let client = reqwest::Client::new();

    // Skip if server is unreachable.
    if client
        .get("http://localhost:8000/v1/models")
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .is_err()
    {
        eprintln!("SKIP: llama-server not reachable at localhost:8000");
        return;
    }

    // Pre-warm Gemma4 — it may need to load from disk (can take ~1 min).
    eprintln!("Pre-warming gemma-4 model...");
    let warm = client
        .post("http://localhost:8000/v1/chat/completions")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "unsloth/gemma-4-26B-A4B-it-GGUF:IQ2_M",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 3,
        }))
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await;
    match warm {
        Ok(r) => eprintln!("Warm-up status: {}", r.status()),
        Err(e) => {
            eprintln!("SKIP: Gemma4 warm-up failed: {e}");
            return;
        }
    }

    // Load config from disk, then patch endpoints to use localhost (host-side).
    let flags = itsy::config::Flags::default();
    let mut config = itsy::config::load_config(&flags);
    config.model.base_url = "http://localhost:8000/v1".into();
    config.second_opinion = itsy::config::SecondOpinionConfig {
        model: Some("unsloth/gemma-4-26B-A4B-it-GGUF:IQ2_M".into()),
        endpoint: Some("http://localhost:8000/v1".into()),
    };

    let brief = "Write a Python function `add(a, b)` that returns the sum of two numbers. \
                 Add a docstring. Include a `__main__` block that prints `add(2, 3)`.";
    let title = "Python add function";
    let main_assertions = vec![
        ("A1".into(), "The file exists at src/add.py".into()),
        ("A2".into(), "Running `python src/add.py` prints 5".into()),
        ("A3".into(), "The function has a docstring".into()),
    ];

    eprintln!("\n=== Starting negotiation ===");
    eprintln!("Main model:   {}", config.model.name);
    eprintln!("Second model: {}", config.second_opinion.model.as_deref().unwrap_or("-"));
    eprintln!("Main assertions ({}):", main_assertions.len());
    for (id, text) in &main_assertions {
        eprintln!("  [{id}] {text}");
    }

    let start = std::time::Instant::now();
    let (result, negotiated) =
        itsy::runtime::features::contract_review::negotiate_assertions(brief, title, main_assertions, &config).await;
    let elapsed = start.elapsed();

    eprintln!("\nNegotiated: {negotiated}  ({:.1}s)", elapsed.as_secs_f64());
    eprintln!("Final assertions ({}):", result.len());
    for (id, text) in &result {
        eprintln!("  [{id}] {text}");
    }

    assert!(!result.is_empty(), "result must not be empty");
    assert!(result.len() <= 24, "must respect 24-assertion cap");
    assert!(negotiated, "negotiation should have run (second opinion is configured)");
    for (id, text) in &result {
        assert!(!id.is_empty(), "id must not be empty");
        assert!(text.len() >= 5, "text too short: {text}");
    }
}
