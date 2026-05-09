//! `whatsapp` — interact with WhatsApp via the whatserve bridge server.
//!
//! Usage:
//!   whatsapp inbox                          list chats; select one to read
//!   whatsapp send <number> "<message>"      send a message
//!   whatsapp setup                          configure server URL and token
//!
//! ## Inbox flow
//! `whatsapp inbox` fetches `/chats` and presents them as a `ListSelector`.
//! Selecting a chat calls `on_list_select("inbox", item, ctx)`, which fetches
//! `/chat/{jid}/messages` and presents the thread as a `ScrollView` (newest
//! message at the top).
//!
//! ## Chat item encoding
//! List items embed the JID so `on_list_select` can recover it:
//!   - Private chat (`@s.whatsapp.net`): item = phone number only.
//!     JID is reconstructed by appending `@s.whatsapp.net`.
//!   - Group chat (`@g.us`): item = `"Name [jid]"`.
//!     JID is extracted from the `[...]` suffix.
//!
//! ## Setup flow
//! Two-step text-prompt: server URL → bearer token → saved to credential store.

use super::{ExecContext, ProgramResult};
use crate::network::{ensure_connectivity, APN};
use serde::Deserialize;

pub const USAGE: &str = "whatsapp inbox | whatsapp send <number> \"<msg>\" | whatsapp setup";

const URL_CONTEXT: &str = "url";
const TOKEN_CONTEXT_PREFIX: &str = "token|";

// --- Entry points -----------------------------------------------------------

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    match args {
        [] => ProgramResult::ok(USAGE.to_string()),
        ["inbox"] => inbox(ctx),
        ["send", number, message] => send(number, message, ctx),
        ["setup"] => start_setup(ctx),
        _ => ProgramResult::err(format!("usage: {USAGE}")),
    }
}

pub fn on_list_select(context: &str, selected: &str, ctx: &mut ExecContext) -> ProgramResult {
    match context {
        "inbox" => open_thread(selected, ctx),
        _ => ProgramResult::err(format!("whatsapp: unexpected context {:?}", context)),
    }
}

pub fn on_text_submit(context: &str, text: &str, ctx: &mut ExecContext) -> ProgramResult {
    if context == URL_CONTEXT {
        return on_url_submit(text, ctx);
    }
    if let Some(url) = context.strip_prefix(TOKEN_CONTEXT_PREFIX) {
        return on_token_submit(url, text, ctx);
    }
    ProgramResult::err(format!("whatsapp: unexpected prompt context {:?}", context))
}

// --- API types --------------------------------------------------------------

#[derive(Deserialize)]
struct ApiResponse<T> {
    code: String,
    message: String,
    results: Option<T>,
}

#[derive(Deserialize)]
struct Paginated<T> {
    data: Vec<T>,
}

#[derive(Deserialize)]
struct Chat {
    jid: String,
    name: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    sender_jid: Option<String>,
    content: Option<String>,
    timestamp: Option<String>,
    is_from_me: Option<bool>,
    media_type: Option<String>,
}

#[derive(serde::Serialize)]
struct SendBody<'a> {
    phone: &'a str,
    message: &'a str,
}

// --- inbox ------------------------------------------------------------------

fn inbox(ctx: &mut ExecContext) -> ProgramResult {
    let creds = match ctx.credentials.whatsapp() {
        Some(c) => c,
        None => return ProgramResult::err("no whatsapp config — run: whatsapp setup".into()),
    };
    if let Err(e) = ensure_connectivity(ctx.wifi, ctx.modem, APN) {
        return ProgramResult::err(format!("no connectivity: {}", e.display()));
    }
    let url = format!("{}/chats?limit=50", creds.base_url);
    let body = match ctx.http.get_with_bearer(&url, &creds.bearer_token) {
        Ok(b) => b,
        Err(e) => return ProgramResult::err(format!("fetch chats: {e}")),
    };
    let parsed: ApiResponse<Paginated<Chat>> = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => return ProgramResult::err(format!("parse error: {e}")),
    };
    if parsed.code != "SUCCESS" {
        return ProgramResult::err(format!("API error: {}", parsed.message));
    }
    let chats = match parsed.results {
        Some(p) => p.data,
        None => return ProgramResult::err("API returned no results".into()),
    };

    // Each item: "display\tvalue" where display is short (fits one screen line)
    // and value is the full JID passed back to on_list_select.
    let items: Vec<String> = chats
        .iter()
        .filter(|c| c.jid != "status@broadcast")
        .map(|c| {
            let display = chat_display_name(c);
            format!("{}\t{}", display, c.jid)
        })
        .collect();

    if items.is_empty() {
        return ProgramResult::ok("no chats".into());
    }

    let mut lines = vec![
        "__START_LIST_VALUED__".to_string(),
        "inbox".to_string(),
        "WhatsApp:".to_string(),
    ];
    lines.extend(items);
    ProgramResult::ok(lines.join("\n"))
}

fn open_thread(jid: &str, ctx: &mut ExecContext) -> ProgramResult {
    let creds = match ctx.credentials.whatsapp() {
        Some(c) => c,
        None => return ProgramResult::err("no whatsapp config".into()),
    };
    let url = format!("{}/chat/{}/messages?limit=30", creds.base_url, jid);
    let body = match ctx.http.get_with_bearer(&url, &creds.bearer_token) {
        Ok(b) => b,
        Err(e) => return ProgramResult::err(format!("fetch messages: {e}")),
    };
    let parsed: ApiResponse<Paginated<Message>> = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => return ProgramResult::err(format!("parse error: {e}")),
    };
    if parsed.code != "SUCCESS" {
        return ProgramResult::err(format!("API error: {}", parsed.message));
    }
    let messages = match parsed.results {
        Some(p) => p.data,
        None => return ProgramResult::err("API returned no results".into()),
    };

    if messages.is_empty() {
        return ProgramResult::ok("no messages".into());
    }

    // API returns newest-first; reverse to chronological order so the newest
    // message lands at the bottom of the content. The scroll view starts at
    // the bottom so the user sees the most recent message immediately.
    let mut content: Vec<String> = Vec::new();
    for msg in messages.iter().rev() {
        let [header, body] = format_message(msg);
        content.push(header);
        content.push(body);
        content.push(String::new()); // blank separator
    }
    content.pop(); // remove trailing blank

    let mut lines = vec!["__START_SCROLL__".to_string(), "bottom".to_string()];
    lines.extend(content);
    ProgramResult::ok(lines.join("\n"))
}

// --- send -------------------------------------------------------------------

fn send(number: &str, message: &str, ctx: &mut ExecContext) -> ProgramResult {
    let creds = match ctx.credentials.whatsapp() {
        Some(c) => c,
        None => return ProgramResult::err("no whatsapp config — run: whatsapp setup".into()),
    };
    if let Err(e) = ensure_connectivity(ctx.wifi, ctx.modem, APN) {
        return ProgramResult::err(format!("no connectivity: {}", e.display()));
    }
    let jid = if number.contains('@') {
        number.to_string()
    } else {
        format!("{}@s.whatsapp.net", number)
    };
    let body = serde_json::to_string(&SendBody { phone: &jid, message })
        .unwrap_or_default();
    let url = format!("{}/send/message", creds.base_url);
    match ctx.http.post_json_with_bearer(&url, &body, &creds.bearer_token) {
        Ok(_) => ProgramResult::ok(format!("sent to {}", number)),
        Err(e) => ProgramResult::err(format!("send failed: {e}")),
    }
}

// --- setup ------------------------------------------------------------------

fn start_setup(ctx: &mut ExecContext) -> ProgramResult {
    let header = match ctx.credentials.whatsapp() {
        Some(creds) => format!("Server URL (current: {}):", creds.base_url),
        None => "Server URL:".to_string(),
    };
    ProgramResult::ok(format!(
        "__START_TEXT_PROMPT__\nplain\n{URL_CONTEXT}\n{header}"
    ))
}

fn on_url_submit(text: &str, ctx: &mut ExecContext) -> ProgramResult {
    let url = text.trim();
    if url.is_empty() {
        return match ctx.credentials.whatsapp() {
            Some(c) => ProgramResult::ok(format!("cancelled — kept {}", c.base_url)),
            None => ProgramResult::err("cannot be empty".into()),
        };
    }
    ProgramResult::ok(format!(
        "__START_TEXT_PROMPT__\nmask\n{TOKEN_CONTEXT_PREFIX}{url}\nBearer token:"
    ))
}

fn on_token_submit(base_url: &str, text: &str, ctx: &mut ExecContext) -> ProgramResult {
    let token = text.trim();
    if token.is_empty() {
        return match ctx.credentials.whatsapp() {
            Some(c) => ProgramResult::ok(format!("cancelled — kept {}", c.base_url)),
            None => ProgramResult::err("cannot be empty".into()),
        };
    }
    match ctx.credentials.set_whatsapp(base_url, token) {
        Ok(()) => ProgramResult::ok(format!("saved whatsapp config: {base_url}")),
        Err(e) => ProgramResult::err(format!("save failed: {e}")),
    }
}

// --- helpers ----------------------------------------------------------------

/// Short display name for a chat — shown in the list selector.
/// The full JID is passed separately as the selection value.
fn chat_display_name(chat: &Chat) -> String {
    if chat.jid.ends_with("@g.us") {
        // Group: show name if available, else the numeric ID.
        let fallback = chat.jid.split('@').next().unwrap_or(&chat.jid);
        chat.name.as_deref().unwrap_or(fallback).to_string()
    } else {
        // Private: phone number (first part of JID).
        chat.jid.split('@').next().unwrap_or(&chat.jid).to_string()
    }
}

/// Format a message as [header_line, body_line].
///
/// Header: `"< 447807555187 [10:30]"` (received) or `"> [10:31]"` (sent).
/// Body:   message text, or `"<photo>"` for media.
fn format_message(msg: &Message) -> [String; 2] {
    let from_me = msg.is_from_me.unwrap_or(false);
    let arrow = if from_me { ">" } else { "<" };

    let time = msg
        .timestamp
        .as_deref()
        .and_then(|ts| ts.splitn(2, 'T').nth(1))
        .and_then(|t| t.splitn(2, '+').next())
        .and_then(|t| t.splitn(2, 'Z').next())
        .unwrap_or("?");
    // Slice by char index to avoid panicking on multi-byte characters in timestamps.
    let time: String = time.chars().take(5).collect(); // HH:MM

    let header = if from_me {
        format!("{} [{}]", arrow, time)
    } else {
        let sender = msg
            .sender_jid
            .as_deref()
            .and_then(|jid| jid.split('@').next())
            .unwrap_or("?");
        format!("{} {} [{}]", arrow, sender, time)
    };

    let media = msg.media_type.as_deref().filter(|s| !s.is_empty());
    let body = if let Some(mt) = media {
        format!("<{}>", mt)
    } else {
        msg.content.as_deref().unwrap_or("").to_string()
    };

    [header, body]
}

// --- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::MockBatteryDriver;
    use crate::charger::MockChargerDriver;
    use crate::credentials::{CredentialStore, MockCredentialStore};
    use crate::email::MockSmtpStreamFactory;
    use crate::http::MockHttpClient;
    use crate::modem::MockModem;
    use crate::saved_networks::MockNetworkStore;
    use crate::wifi::{MockWifiDriver, WifiDriver};

    const BASE_URL: &str = "https://wa.example.com";
    const TOKEN: &str = "secret-token";

    struct Env {
        wifi: MockWifiDriver,
        http: MockHttpClient,
        saved: MockNetworkStore,
        smtp: MockSmtpStreamFactory,
        creds: MockCredentialStore,
        modem: MockModem,
        battery: MockBatteryDriver,
        charger: MockChargerDriver,
    }

    impl Env {
        fn new() -> Self {
            let mut env = Self {
                wifi: MockWifiDriver::new(),
                http: MockHttpClient::new(),
                saved: MockNetworkStore::new(),
                smtp: MockSmtpStreamFactory::new(),
                creds: MockCredentialStore::new(),
                modem: MockModem::new(),
                battery: MockBatteryDriver::new(),
                charger: MockChargerDriver::new(),
            };
            env.wifi.connect("home_wifi", "").unwrap();
            env
        }

        fn with_creds(mut self) -> Self {
            self.creds.set_whatsapp(BASE_URL, TOKEN).unwrap();
            self
        }

        fn ctx(&mut self) -> ExecContext<'_> {
            ExecContext {
                uptime_secs: 0,
                wifi: &mut self.wifi,
                http: &mut self.http,
                saved_networks: &mut self.saved,
                smtp: &mut self.smtp,
                credentials: &mut self.creds,
                modem: &mut self.modem,
                battery: &mut self.battery,
                charger: &mut self.charger,
            }
        }
    }

    fn chat_list_json() -> &'static str {
        r#"{"code":"SUCCESS","message":"ok","results":{"data":[
            {"jid":"447807555187@s.whatsapp.net","name":"Alice"},
            {"jid":"120363420474396842@g.us","name":"Family"},
            {"jid":"status@broadcast","name":null}
        ],"pagination":{"limit":50,"offset":0,"total":3}}}"#
    }

    fn messages_json() -> &'static str {
        // API returns newest-first (most recent message is data[0]).
        r#"{"code":"SUCCESS","message":"ok","results":{"data":[
            {"sender_jid":"me@s.whatsapp.net","content":"hi there","timestamp":"2024-01-15T10:31:00Z","is_from_me":true,"media_type":""},
            {"sender_jid":"447807555187@s.whatsapp.net","content":"hello","timestamp":"2024-01-15T10:30:00Z","is_from_me":false,"media_type":""}
        ],"pagination":{"limit":30,"offset":0,"total":2}}}"#
    }

    // --- no-args ---

    #[test]
    fn no_args_returns_usage() {
        let mut env = Env::new();
        let r = run(&[], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, USAGE);
    }

    // --- inbox ---

    #[test]
    fn inbox_no_creds_hints_setup() {
        let mut env = Env::new();
        let r = run(&["inbox"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("whatsapp setup"));
    }

    #[test]
    fn inbox_formats_chat_list() {
        let mut env = Env::new().with_creds();
        env.http.on_get_with_bearer(
            &format!("{BASE_URL}/chats?limit=50"),
            Ok(chat_list_json().into()),
        );
        let r = run(&["inbox"], &mut env.ctx());
        assert_eq!(r.exit_code, 0, "output: {}", r.output);
        assert!(r.output.starts_with("__START_LIST_VALUED__\ninbox\nWhatsApp:"));
        // Private chat: display is phone number, value is full JID
        assert!(r.output.contains("447807555187\t447807555187@s.whatsapp.net"));
        // Group chat: display is just the name, value is full JID
        assert!(r.output.contains("Family\t120363420474396842@g.us"));
        // status@broadcast filtered out
        assert!(!r.output.contains("status@broadcast"));
    }

    #[test]
    fn inbox_http_error_reported() {
        let mut env = Env::new().with_creds();
        env.http.on_get_with_bearer(
            &format!("{BASE_URL}/chats?limit=50"),
            Err("connection refused".into()),
        );
        let r = run(&["inbox"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("fetch chats"));
    }

    // --- on_list_select / open_thread ---

    #[test]
    fn select_private_chat_fetches_messages() {
        let mut env = Env::new().with_creds();
        env.http.on_get_with_bearer(
            &format!("{BASE_URL}/chat/447807555187@s.whatsapp.net/messages?limit=30"),
            Ok(messages_json().into()),
        );
        // on_list_select now receives the JID directly as the value
        let r = on_list_select("inbox", "447807555187@s.whatsapp.net", &mut env.ctx());
        assert_eq!(r.exit_code, 0, "output: {}", r.output);
        assert!(r.output.starts_with("__START_SCROLL__\nbottom\n"));
        let content = r.output.strip_prefix("__START_SCROLL__\nbottom\n").unwrap();
        let lines: Vec<&str> = content.lines().collect();
        // API is newest-first; .rev() produces chronological order in content.
        // Oldest (10:30 received) is first in content, newest (10:31 sent) is last.
        assert!(lines[0].contains("<"), "received arrow: {}", lines[0]);
        assert!(lines[0].contains("10:30"), "oldest first: {}", lines[0]);
        assert_eq!(lines[1], "hello");
        assert_eq!(lines[2], "");
        assert!(lines[3].contains(">"), "sent arrow: {}", lines[3]);
        assert!(lines[3].contains("10:31"), "newest last: {}", lines[3]);
        assert_eq!(lines[4], "hi there");
    }

    #[test]
    fn select_group_chat_receives_jid_directly() {
        let mut env = Env::new().with_creds();
        env.http.on_get_with_bearer(
            &format!("{BASE_URL}/chat/120363420474396842@g.us/messages?limit=30"),
            Ok(messages_json().into()),
        );
        let r = on_list_select("inbox", "120363420474396842@g.us", &mut env.ctx());
        assert_eq!(r.exit_code, 0, "output: {}", r.output);
        assert!(r.output.starts_with("__START_SCROLL__"));
    }

    // --- send ---

    #[test]
    fn send_no_creds_hints_setup() {
        let mut env = Env::new();
        let r = run(&["send", "447807555187", "hello"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("whatsapp setup"));
    }

    #[test]
    fn send_posts_message() {
        let mut env = Env::new().with_creds();
        env.http.on_post_json_with_bearer(
            &format!("{BASE_URL}/send/message"),
            Ok(r#"{"code":"SUCCESS","message":"ok","results":{"message_id":"x","status":"sent"}}"#.into()),
        );
        let r = run(&["send", "447807555187", "hello"], &mut env.ctx());
        assert_eq!(r.exit_code, 0, "output: {}", r.output);
        assert!(r.output.contains("sent to 447807555187"));
    }

    #[test]
    fn send_number_with_at_passes_jid_through() {
        let mut env = Env::new().with_creds();
        env.http.on_post_json_with_bearer(
            &format!("{BASE_URL}/send/message"),
            Ok(r#"{"code":"SUCCESS","message":"ok","results":null}"#.into()),
        );
        let r = run(&["send", "447807555187@s.whatsapp.net", "hi"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
    }

    // --- setup ---

    #[test]
    fn setup_starts_url_prompt() {
        let mut env = Env::new();
        let r = run(&["setup"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        let lines: Vec<&str> = r.output.lines().collect();
        assert_eq!(lines[0], "__START_TEXT_PROMPT__");
        assert_eq!(lines[1], "plain");
        assert_eq!(lines[2], "url");
        assert_eq!(lines[3], "Server URL:");
    }

    #[test]
    fn setup_url_submit_prompts_for_token() {
        let mut env = Env::new();
        let r = on_text_submit("url", "https://wa.example.com", &mut env.ctx());
        let lines: Vec<&str> = r.output.lines().collect();
        assert_eq!(lines[0], "__START_TEXT_PROMPT__");
        assert_eq!(lines[1], "mask");
        assert_eq!(lines[2], "token|https://wa.example.com");
        assert_eq!(lines[3], "Bearer token:");
    }

    #[test]
    fn setup_token_submit_saves_and_confirms() {
        let mut env = Env::new();
        let r = on_text_submit("token|https://wa.example.com", "my-token", &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("saved whatsapp config"));
        let creds = env.creds.whatsapp().unwrap();
        assert_eq!(creds.base_url, "https://wa.example.com");
        assert_eq!(creds.bearer_token, "my-token");
    }

    #[test]
    fn setup_empty_url_rejected_when_no_creds() {
        let mut env = Env::new();
        let r = on_text_submit("url", "", &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("empty"));
    }

    // --- helpers ---

    #[test]
    fn chat_display_name_private() {
        let chat = Chat { jid: "447807555187@s.whatsapp.net".into(), name: Some("Alice".into()) };
        assert_eq!(chat_display_name(&chat), "447807555187");
    }

    #[test]
    fn chat_display_name_group_with_name() {
        let chat = Chat { jid: "123@g.us".into(), name: Some("Family".into()) };
        assert_eq!(chat_display_name(&chat), "Family");
    }

    #[test]
    fn chat_display_name_group_no_name() {
        let chat = Chat { jid: "123@g.us".into(), name: None };
        assert_eq!(chat_display_name(&chat), "123");
    }
}
