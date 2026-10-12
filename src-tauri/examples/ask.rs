use voicekeyboard::core::gemini::{ChatMessage, GeminiClient};

fn main() {
    let dir = voicekeyboard::app_data_dir();
    let raw = std::fs::read_to_string(dir.join("settings.json")).expect("settings.json");
    let s: serde_json::Value = serde_json::from_str(&raw).expect("parse settings");
    let key = s["geminiApiKey"].as_str().unwrap_or_default().to_string();
    let model = s["llmModel"].as_str().unwrap_or("").to_string();
    let prompt = s["llmSystemPrompt"].as_str().unwrap_or("").to_string();
    let tts_model = s["ttsModel"].as_str().unwrap_or("").to_string();
    let voice = s["ttsVoice"].as_str().unwrap_or("").to_string();
    if key.is_empty() {
        eprintln!("no gemini api key in settings");
        std::process::exit(1);
    }
    println!("model={model} system_prompt_len={}", prompt.len());
    let client = GeminiClient::new(&key, &model, &prompt, &tts_model, &voice);
    let question = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "What is the weather in Porto Alegre today?".into());
    println!("question: {question}");
    let messages = vec![ChatMessage {
        role: "user".into(),
        text: question,
    }];
    match client.chat(&messages) {
        Ok(o) => {
            println!("REPLY: {}", o.text);
            let titles: Vec<String> = o.sources.iter().map(|s| s.title.clone()).collect();
            println!("SOURCES[{}]: {}", titles.len(), titles.join(" | "));
        }
        Err(e) => println!("ERROR: {e}"),
    }
}
