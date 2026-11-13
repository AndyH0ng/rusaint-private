use std::sync::LazyLock;

use base64::{Engine, engine::general_purpose::STANDARD};
use rmcp::model::Icon;

pub const ICON: &[u8] = include_bytes!("../assets/icon.png");

static ICON_URI: LazyLock<String> =
    LazyLock::new(|| format!("data:image/png;base64,{}", STANDARD.encode(ICON)));

pub fn mcp_icon() -> Icon {
    Icon::new(ICON_URI.as_str())
        .with_mime_type("image/png")
        .with_sizes(vec!["464x464".into()])
}
