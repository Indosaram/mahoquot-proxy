use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Dashboard = 0,
    Config = 1,
    Accounts = 2,
    Quota = 3,
    Logs = 4,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Dashboard,
        Tab::Config,
        Tab::Accounts,
        Tab::Quota,
        Tab::Logs,
    ];

    pub fn title(&self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Config => "Configuration",
            Tab::Accounts => "Accounts",
            Tab::Quota => "Quota & Diagnostics",
            Tab::Logs => "Logs",
        }
    }

    pub fn next(&self) -> Self {
        match self {
            Tab::Dashboard => Tab::Config,
            Tab::Config => Tab::Accounts,
            Tab::Accounts => Tab::Quota,
            Tab::Quota => Tab::Logs,
            Tab::Logs => Tab::Dashboard,
        }
    }

    pub fn prev(&self) -> Self {
        match self {
            Tab::Dashboard => Tab::Logs,
            Tab::Config => Tab::Dashboard,
            Tab::Accounts => Tab::Config,
            Tab::Quota => Tab::Accounts,
            Tab::Logs => Tab::Quota,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ManagementClient {
    base_url: String,
    secret_key: String,
    client: reqwest::Client,
}

impl ManagementClient {
    pub fn new(base_url: String, secret_key: String) -> Self {
        let base = base_url.trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self {
            base_url: base,
            secret_key: secret_key.trim().to_string(),
            client,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn set_secret_key(&mut self, key: String) {
        self.secret_key = key.trim().to_string();
    }

    pub async fn get_config(&self) -> Result<Value, String> {
        let url = format!("{}/v0/management/config", self.base_url);
        let mut req = self.client.get(&url);
        if !self.secret_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.secret_key));
            req = req.header("X-Management-Key", &self.secret_key);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    pub async fn get_auth_files(&self) -> Result<Vec<Value>, String> {
        let url = format!("{}/v0/management/auth-files", self.base_url);
        let mut req = self.client.get(&url);
        if !self.secret_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.secret_key));
            req = req.header("X-Management-Key", &self.secret_key);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let val: Value = resp.json().await.map_err(|e| e.to_string())?;
        if let Some(files) = val.get("files").and_then(Value::as_array) {
            Ok(files.clone())
        } else if let Some(arr) = val.as_array() {
            Ok(arr.clone())
        } else {
            Ok(vec![])
        }
    }

    pub async fn reset_quota(&self, auth_index: &str) -> Result<(), String> {
        let url = format!("{}/v0/management/reset-quota", self.base_url);
        let body = serde_json::json!({ "auth_index": auth_index });
        let mut req = self.client.post(&url).json(&body);
        if !self.secret_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.secret_key));
            req = req.header("X-Management-Key", &self.secret_key);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        Ok(())
    }

    pub async fn get_logs(&self) -> Result<Vec<String>, String> {
        let url = format!("{}/v0/management/logs", self.base_url);
        let mut req = self.client.get(&url);
        if !self.secret_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.secret_key));
            req = req.header("X-Management-Key", &self.secret_key);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let val: Value = resp.json().await.map_err(|e| e.to_string())?;
        if let Some(lines) = val.get("lines").and_then(Value::as_array) {
            let res: Vec<String> = lines
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.to_string())
                .collect();
            Ok(res)
        } else {
            Ok(vec![])
        }
    }
}

#[derive(Default, Clone)]
pub struct LogBuffer {
    lines: Arc<Mutex<VecDeque<String>>>,
    max_capacity: usize,
}

impl LogBuffer {
    pub fn new(max_capacity: usize) -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::with_capacity(max_capacity))),
            max_capacity,
        }
    }

    pub fn push(&self, line: String) {
        let mut q = self.lines.lock().unwrap();
        if q.len() >= self.max_capacity {
            q.pop_front();
        }
        q.push_back(line);
    }

    pub fn snapshot(&self) -> Vec<String> {
        let q = self.lines.lock().unwrap();
        q.iter().cloned().collect()
    }
}

pub struct TuiState {
    pub active_tab: Tab,
    pub client: ManagementClient,
    pub authenticated: bool,
    pub auth_error: Option<String>,
    pub password_input: String,
    pub is_standalone: bool,
    pub log_buffer: Option<LogBuffer>,
    pub cached_config: Option<Value>,
    pub cached_accounts: Vec<Value>,
    pub cached_logs: Vec<String>,
    pub status_message: Option<String>,
    pub width: u16,
    pub height: u16,
}

impl TuiState {
    pub fn new(
        client: ManagementClient,
        is_standalone: bool,
        log_buffer: Option<LogBuffer>,
    ) -> Self {
        let initial_auth = is_standalone || !client.secret_key.is_empty();
        Self {
            active_tab: Tab::Dashboard,
            client,
            authenticated: initial_auth,
            auth_error: None,
            password_input: String::new(),
            is_standalone,
            log_buffer,
            cached_config: None,
            cached_accounts: Vec::new(),
            cached_logs: Vec::new(),
            status_message: None,
            width: 80,
            height: 24,
        }
    }

    pub fn handle_key(&mut self, key: &str) -> bool {
        if !self.authenticated {
            match key {
                "q" | "ctrl+c" => return true,
                "enter" => {
                    let entered = self.password_input.trim().to_string();
                    if entered.is_empty() {
                        self.auth_error = Some("Password is required".into());
                    } else {
                        self.client.set_secret_key(entered);
                        self.authenticated = true;
                        self.auth_error = None;
                    }
                }
                "backspace" => {
                    self.password_input.pop();
                }
                c if c.len() == 1 => {
                    self.password_input.push_str(c);
                }
                _ => {}
            }
            return false;
        }

        match key {
            "q" | "ctrl+c" => true,
            "tab" => {
                self.active_tab = self.active_tab.next();
                false
            }
            "shift+tab" => {
                self.active_tab = self.active_tab.prev();
                false
            }
            "1" => {
                self.active_tab = Tab::Dashboard;
                false
            }
            "2" => {
                self.active_tab = Tab::Config;
                false
            }
            "3" => {
                self.active_tab = Tab::Accounts;
                false
            }
            "4" => {
                self.active_tab = Tab::Quota;
                false
            }
            "5" => {
                self.active_tab = Tab::Logs;
                false
            }
            "r" => {
                self.status_message = Some("Refreshing screen...".into());
                false
            }
            _ => false,
        }
    }

    pub fn render_screen(&self) -> String {
        let mut out = String::new();

        if !self.authenticated {
            out.push_str("┌────────────────────────────────────────────────────────────┐\n");
            out.push_str("│             Mahoquot Gateway Management Login              │\n");
            out.push_str("├────────────────────────────────────────────────────────────┤\n");
            out.push_str("│ Password: ");
            let masked = "*".repeat(self.password_input.len());
            out.push_str(&masked);
            out.push_str("_\n");
            if let Some(ref err) = self.auth_error {
                out.push_str(&format!("│ Error: {}\n", err));
            }
            out.push_str("│                                                            │\n");
            out.push_str("│ Press Enter to connect, 'q' or Ctrl+C to quit.             │\n");
            out.push_str("└────────────────────────────────────────────────────────────┘\n");
            return out;
        }

        out.push_str("┌─");
        for t in &Tab::ALL {
            if *t == self.active_tab {
                out.push_str(&format!("[* {} *]─", t.title()));
            } else {
                out.push_str(&format!("  {}  ─", t.title()));
            }
        }
        out.push_str("┐\n");

        match self.active_tab {
            Tab::Dashboard => self.render_dashboard(&mut out),
            Tab::Config => self.render_config(&mut out),
            Tab::Accounts => self.render_accounts(&mut out),
            Tab::Quota => self.render_quota(&mut out),
            Tab::Logs => self.render_logs(&mut out),
        }

        out.push_str("├────────────────────────────────────────────────────────────┤\n");
        out.push_str("│ Tab/Shift+Tab: switch tabs | 1-5: quick jump | r: refresh  │\n");
        if let Some(ref msg) = self.status_message {
            out.push_str(&format!("│ Status: {} \n", msg));
        }
        out.push_str("└────────────────────────────────────────────────────────────┘\n");

        out
    }

    fn render_dashboard(&self, out: &mut String) {
        out.push_str("│ SCREEN: DASHBOARD                                          │\n");
        out.push_str(&format!("│ Target URL: {} \n", self.client.base_url()));
        let mode = if self.is_standalone {
            "Embedded Local Service"
        } else {
            "Attached Remote Client"
        };
        out.push_str(&format!("│ Mode: {} \n", mode));
        out.push_str(&format!(
            "│ Accounts Registered: {} \n",
            self.cached_accounts.len()
        ));
        let (active, disabled) = self.account_stats();
        out.push_str(&format!(
            "│ Active: {} | Disabled/Quota: {} \n",
            active, disabled
        ));
    }

    fn render_config(&self, out: &mut String) {
        out.push_str("│ SCREEN: CONFIGURATION                                      │\n");
        if let Some(ref cfg) = self.cached_config {
            let port = cfg.get("port").and_then(Value::as_u64).unwrap_or(0);
            let strategy = cfg
                .get("strategy")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let retry = cfg
                .get("request-retry")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            out.push_str(&format!("│ Port: {} \n", port));
            out.push_str(&format!("│ Strategy: {} \n", strategy));
            out.push_str(&format!("│ Request Retry: {} \n", retry));
        } else {
            out.push_str("│ Config: [Not Loaded or Clean Defaults]                    │\n");
        }
    }

    fn render_accounts(&self, out: &mut String) {
        out.push_str("│ SCREEN: ACCOUNTS LIST                                      │\n");
        if self.cached_accounts.is_empty() {
            out.push_str("│ No provider accounts found.                                │\n");
        } else {
            for (idx, acc) in self.cached_accounts.iter().enumerate().take(8) {
                let name = acc.get("name").and_then(Value::as_str).unwrap_or("unknown");
                let provider = acc
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or("generic");
                let disabled = acc
                    .get("disabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let status_str = if disabled { "DISABLED" } else { "ACTIVE" };
                out.push_str(&format!(
                    "│ [{}] {} ({}) - {} \n",
                    idx + 1,
                    name,
                    provider,
                    status_str
                ));
            }
        }
    }

    fn render_quota(&self, out: &mut String) {
        out.push_str("│ SCREEN: QUOTA & DIAGNOSTICS                                │\n");
        out.push_str("│ Provider rate limit, cooldowns, and reset diagnostics:     │\n");
        let mut found_quota = false;
        for acc in &self.cached_accounts {
            if let Some(quota) = acc.get("quota") {
                if let Some(idx) = acc.get("auth_index").and_then(Value::as_str) {
                    let prov = acc
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let reason = quota
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("none");
                    out.push_str(&format!(
                        "│ AuthIndex: {} ({}) Reason: {}\n",
                        idx, prov, reason
                    ));
                    found_quota = true;
                }
            }
        }
        if !found_quota {
            out.push_str("│ All registered credentials healthy with no quota blocks.   │\n");
        }
    }

    fn render_logs(&self, out: &mut String) {
        out.push_str("│ SCREEN: LOGS MONITOR                                       │\n");
        let lines: Vec<String> = if let Some(ref buffer) = self.log_buffer {
            buffer.snapshot()
        } else {
            self.cached_logs.clone()
        };

        if lines.is_empty() {
            out.push_str("│ [No log lines recorded]                                    │\n");
        } else {
            for line in lines.iter().rev().take(6) {
                let truncated = if line.len() > 56 { &line[..56] } else { line };
                out.push_str(&format!("│ > {} \n", truncated));
            }
        }
    }

    fn account_stats(&self) -> (usize, usize) {
        let mut active = 0;
        let mut disabled = 0;
        for a in &self.cached_accounts {
            if a.get("disabled").and_then(Value::as_bool).unwrap_or(false) {
                disabled += 1;
            } else {
                active += 1;
            }
        }
        (active, disabled)
    }

    pub async fn refresh_data(&mut self) {
        if !self.authenticated {
            return;
        }

        if let Ok(cfg) = self.client.get_config().await {
            self.cached_config = Some(cfg);
        }

        if let Ok(accs) = self.client.get_auth_files().await {
            self.cached_accounts = accs;
        }

        if let Ok(logs) = self.client.get_logs().await {
            self.cached_logs = logs;
        }
    }
}

pub struct PtySessionSimulator {
    pub state: TuiState,
    pub transcript: Vec<String>,
}

impl PtySessionSimulator {
    pub fn new(state: TuiState) -> Self {
        Self {
            state,
            transcript: Vec::new(),
        }
    }

    pub fn send_key(&mut self, key: &str) -> bool {
        let should_quit = self.state.handle_key(key);
        let screen = self.state.render_screen();
        self.transcript.push(format!("KEY[{}]:\n{}", key, screen));
        should_quit
    }

    pub fn current_screen(&self) -> String {
        self.state.render_screen()
    }
}

pub async fn run_interactive_tui(mut state: TuiState) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::io::{Read, Write};
        let stdin_fd = libc::STDIN_FILENO;
        let is_tty = unsafe { libc::isatty(stdin_fd) == 1 };

        state.refresh_data().await;

        if !is_tty {
            // Non-interactive or test runner mode: print screen and exit cleanly
            print!("{}", state.render_screen());
            std::io::stdout().flush().ok();
            return Ok(());
        }

        // TTY Interactive Mode with raw terminal setup
        let mut orig_termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        let tc_res = unsafe { libc::tcgetattr(stdin_fd, orig_termios.as_mut_ptr()) };
        let raw = unsafe { orig_termios.assume_init() };

        if tc_res == 0 {
            let mut raw_copy = raw;
            unsafe {
                libc::cfmakeraw(&mut raw_copy);
                libc::tcsetattr(stdin_fd, libc::TCSANOW, &raw_copy);
            }
        }

        struct RawModeGuard {
            fd: i32,
            orig: libc::termios,
            active: bool,
        }

        impl Drop for RawModeGuard {
            fn drop(&mut self) {
                if self.active {
                    unsafe {
                        libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
                    }
                }
            }
        }

        let _guard = RawModeGuard {
            fd: stdin_fd,
            orig: raw,
            active: tc_res == 0,
        };

        // Clear screen and show initial state
        print!(
            "\x1B[2J\x1B[H{}",
            state.render_screen().replace('\n', "\r\n")
        );
        std::io::stdout().flush().ok();

        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 32];

        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };

            let key_str = match &buf[..n] {
                b"q" | b"Q" | &[3] => break, // 'q' or Ctrl+C
                b"\t" => "tab",
                b"\x1b[Z" => "shift+tab",
                b"\r" | b"\n" => "enter",
                b"\x7f" | b"\x08" => "backspace",
                b"1" => "1",
                b"2" => "2",
                b"3" => "3",
                b"4" => "4",
                b"5" => "5",
                b"r" | b"R" => "r",
                other => std::str::from_utf8(other).unwrap_or_default(),
            };

            if key_str == "r" {
                state.refresh_data().await;
            }

            let should_quit = state.handle_key(key_str);

            print!(
                "\x1B[2J\x1B[H{}",
                state.render_screen().replace('\n', "\r\n")
            );
            std::io::stdout().flush().ok();

            if should_quit {
                break;
            }
        }

        println!("\r\nExiting Mahoquot TUI.");
        Ok(())
    }

    #[cfg(not(unix))]
    {
        state.refresh_data().await;
        println!("{}", state.render_screen());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tui_tab_cycling() {
        let t = Tab::Dashboard;
        assert_eq!(t.next(), Tab::Config);
        assert_eq!(t.next().next(), Tab::Accounts);
        assert_eq!(t.next().next().next(), Tab::Quota);
        assert_eq!(t.next().next().next().next(), Tab::Logs);
        assert_eq!(t.next().next().next().next().next(), Tab::Dashboard);
        assert_eq!(t.prev(), Tab::Logs);
    }

    #[test]
    fn test_pty_navigation_and_rendering() {
        let client = ManagementClient::new("http://127.0.0.1:18801".into(), "test-key".into());
        let tui_state = TuiState::new(client, false, None);
        let mut pty = PtySessionSimulator::new(tui_state);

        assert!(pty.state.authenticated);
        let s0 = pty.current_screen();
        assert!(s0.contains("SCREEN: DASHBOARD"));

        pty.send_key("tab");
        assert_eq!(pty.state.active_tab, Tab::Config);
        assert!(pty.current_screen().contains("SCREEN: CONFIGURATION"));

        pty.send_key("tab");
        assert_eq!(pty.state.active_tab, Tab::Accounts);
        assert!(pty.current_screen().contains("SCREEN: ACCOUNTS LIST"));

        pty.send_key("tab");
        assert_eq!(pty.state.active_tab, Tab::Quota);
        assert!(pty.current_screen().contains("SCREEN: QUOTA & DIAGNOSTICS"));

        pty.send_key("tab");
        assert_eq!(pty.state.active_tab, Tab::Logs);
        assert!(pty.current_screen().contains("SCREEN: LOGS MONITOR"));

        pty.send_key("1");
        assert_eq!(pty.state.active_tab, Tab::Dashboard);

        let quit = pty.send_key("q");
        assert!(quit);
    }

    #[test]
    fn test_tui_unauthenticated_login_flow() {
        let client = ManagementClient::new("http://127.0.0.1:18801".into(), "".into());
        let tui_state = TuiState::new(client, false, None);
        let mut pty = PtySessionSimulator::new(tui_state);

        assert!(!pty.state.authenticated);
        let s0 = pty.current_screen();
        assert!(s0.contains("Mahoquot Gateway Management Login"));

        pty.send_key("s");
        pty.send_key("e");
        pty.send_key("c");
        pty.send_key("r");
        pty.send_key("e");
        pty.send_key("t");
        assert_eq!(pty.state.password_input, "secret");

        pty.send_key("enter");
        assert!(pty.state.authenticated);
        assert_eq!(pty.state.client.secret_key, "secret");
        assert!(pty.current_screen().contains("SCREEN: DASHBOARD"));
    }
}
