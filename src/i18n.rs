const LOCALE_OVERRIDE_ARGS: &[&str] = &["--lang", "--locale", "/lang", "/locale"];

/// 初始化 i18n：检测系统语言（或启动参数覆盖），设置全局 locale
pub fn init() {
    let locale = locale_from_args().unwrap_or_else(detect_locale);
    rust_i18n::set_locale(&locale);
    tracing::info!("i18n locale set to: {}", locale);
}

/// 检查启动参数中是否有 --lang / --locale 覆盖
fn locale_from_args() -> Option<String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    for (i, arg) in args.iter().enumerate() {
        let lower = arg.to_ascii_lowercase();
        if LOCALE_OVERRIDE_ARGS.iter().any(|flag| lower == *flag) {
            if let Some(value) = args.get(i + 1) {
                return Some(normalize_locale(value));
            }
        }
        // 支持 --lang=zh-CN 形式
        if let Some(value) = lower.strip_prefix("--lang=") {
            return Some(normalize_locale(value));
        }
        if let Some(value) = lower.strip_prefix("--locale=") {
            return Some(normalize_locale(value));
        }
    }
    None
}

/// 将用户输入的语言代码标准化为支持的 locale
fn normalize_locale(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    if lower.starts_with("zh") {
        if lower.contains("tw")
            || lower.contains("hk")
            || lower.contains("hant")
            || lower == "zh-tw"
            || lower == "zht"
        {
            "zh-TW".to_string()
        } else {
            "zh-CN".to_string()
        }
    } else if lower.starts_with("ja") || lower == "jp" {
        "ja".to_string()
    } else if lower.starts_with("en") || lower == "us" {
        "en".to_string()
    } else {
        // 无法识别的语言回退到英语
        tracing::warn!("Unknown locale '{}', falling back to 'en'", input);
        "en".to_string()
    }
}

/// 检测系统语言，返回对应的 locale 字符串
fn detect_locale() -> String {
    let sys_locale = sys_locale::get_locale().unwrap_or_else(|| "en".to_string());

    let lang = sys_locale.to_lowercase();
    if lang.starts_with("zh") {
        if lang.contains("tw") || lang.contains("hk") || lang.contains("hant") {
            "zh-TW".to_string()
        } else {
            "zh-CN".to_string()
        }
    } else if lang.starts_with("ja") {
        "ja".to_string()
    } else {
        "en".to_string()
    }
}
